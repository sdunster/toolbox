# Database Schema Reference

Toolbox uses DynamoDB as its database backend. All tables are defined in `infra/dynamodb.tf` —
that file, not this one, is the source of truth for the deployed tables; `api/src/bin/local-tables.rs`
is transcribed from it by hand for local development and must be kept in sync (`make
local-tables-check` in CI is the tripwire). This project is prod-only, so unlike some sibling
projects there is no parallel test-prefix table set: every table here is named
`{DB_PREFIX}_{entity}`, and `DB_PREFIX` is `prod` in the deployed stack, `local` in local
development.

All tables use `PAY_PER_REQUEST` billing (on-demand capacity); there is no provisioned throughput
to tune. Hash keys are a 12-char nanoid unless noted otherwise (a few tables — `inbound_address`,
`login_code`, `processed_message` — key on a natural value instead, called out below).

DynamoDB only enforces uniqueness on the primary key. All other uniqueness requirements (e.g. one
`slug` per instance) are enforced at the application layer.

In DynamoDB, only attributes that are part of a table key or a GSI key must be declared in the
table definition. All other fields are schema-free per item — they exist because application code
writes them, not because DynamoDB requires them.

All IDs are exposed to the API layer as opaque strings. Conversion/validation happens inside
`dynamodb.rs`, never in callers.

**House rule (see `CLAUDE.md`): optional attributes are omitted, never written as `Null`.**
Clearing an optional value means removing the attribute (`REMOVE` in an update expression), not
setting it to null — this matters most for the sparse-GSI marker attributes below, where an
absent attribute is what drops a row out of an index.

---

## Schema

### `{prefix}_instance`

| Attribute | Type | Role                  |
| --------- | ---- | --------------------- |
| `id`      | S    | Hash key (PK) — nanoid |
| `slug`    | S    | GSI hash key           |

**GSIs:**

| GSI           | Hash key | Sort key | Projection | Purpose                                                                          |
| ------------- | -------- | -------- | ---------- | --------------------------------------------------------------------------------- |
| `slug-index`  | `slug`   | —        | KEYS_ONLY  | Resolve a URL slug (`/app/:slug`, `/submit/:slug`) to an instance id             |

`KEYS_ONLY` is deliberately minimal: the resolved id drives a separate `GetItem` for the full
instance record, so projecting more into the index would just be wasted storage.

**Non-obvious attributes (not in the table definition):**

- `name` (S) — display name
- `kind` (S) — `"invoicing"` only; absent means `"support"`. Set once at creation
  (`createInstance`/`instance create --kind`), **immutable after creation** — no mutation and no
  CLI command changes it. See CLAUDE.md's "Invoicing" house rule for the kind-isolation rule this
  drives (a support-only operation rejects an invoicing instance identically to a missing/deleted
  one; an invoicing-only operation rejects a support instance with a plain validation error).
- `from_name` (S) — the `From:` display name used on outbound mail
- `signature` (S) — appended to outbound replies
- `public_submission_enabled` (Bool) — opt-in, default off/absent; gates whether the instance
  appears in the bare `/submit` list. Support instances only in practice — `publicInstances`/
  `requestSubmitCode` both also require `kind = support`.
- `business_name`, `business_abn`, `business_address`, `business_phone`, `business_email`,
  `payment_details` (all S) — invoicing settings: the seller details and payment footer an invoice
  prints. Invoicing instances only (nothing enforces that at the storage layer; every write path
  that could set them, `updateInvoicingSettings`, rejects a support instance first). Full-replace
  write: every call to `updateInvoicingSettings` writes every field, and a blank string `REMOVE`s
  the attribute rather than storing an empty one.
- `gst_registered` (Bool) — invoicing setting, only ever written `true`; absent means not
  registered for GST, same omit convention as `deleted` below.
- `currency` (S) — invoicing setting; absent means `"AUD"` (`db::Instance::currency_or_default`).
  A non-blank value must be 3 uppercase ASCII letters (`db::validate_currency_code`).
- `deleted` (Bool) — soft-delete marker, same omit convention: only ever
  written `true`; absent means active. Set/cleared only via
  `InstanceUpdateShape::SetDeleted` (`setInstanceDeleted`/`bin/cli.rs`'s
  `instance update --deleted`). A deleted instance is hidden from every
  ordinary path a member or requester reaches it through:
  - `User.memberships` filters it out — a former member's instance switcher
    stops showing it.
  - `Query.instance(slug)` resolves to `null` for it, identically to a slug
    that doesn't exist or one the caller isn't a member of (no probing).
  - Inbound mail addressed to it is dropped the same way mail to no known
    instance is (logged, no ticket opened, no mail sent) — see
    `inbound::pipeline::process_raw_message`'s step 4.
  `adminInstances`/`adminInstance` (superuser-only) deliberately do **not**
  filter it — restoring a deleted instance is what those queries are for.
  See "Known issues and risks" below for what this does *not* close: a
  former member with a ticket id already in hand.

---

### `{prefix}_inbound_address`

| Attribute     | Type | Role                                              |
| ------------- | ---- | -------------------------------------------------- |
| `address`     | S    | Hash key (PK) — lowercased full address or `*@domain` wildcard |
| `instance_id` | S    | GSI hash key                                        |

The hash key is a natural value, not an opaque id: inbound-mail routing does a direct `GetItem`
on the lowercased, `+tag`-stripped recipient address, then falls back to `*@domain`. Making the
address itself the key means routing is a `GetItem`, not a `Query`.

**GSIs:**

| GSI                 | Hash key      | Sort key | Projection | Purpose                                                        |
| ------------------- | ------------- | -------- | ---------- | ---------------------------------------------------------------- |
| `instance_id-index` | `instance_id` | —        | ALL        | Instance settings page: list every address owned by an instance |

`ALL`: the settings UI renders `address`/`kind`/`created_at` directly from the list, and an
instance has at most a handful of addresses, so the per-item storage cost of projecting
everything is negligible next to avoiding an N-way `BatchGetItem`.

**Non-obvious attributes:**

- `kind` (S) — e.g. `exact` or `wildcard`
- `created_at` (N) — Unix timestamp

---

### `{prefix}_user`

| Attribute | Type | Role                  |
| --------- | ---- | --------------------- |
| `id`      | S    | Hash key (PK) — nanoid |
| `email`   | S    | GSI hash key           |

**GSIs:**

| GSI            | Hash key | Sort key | Projection | Purpose                                                                    |
| -------------- | -------- | -------- | ---------- | ---------------------------------------------------------------------------- |
| `email-index`  | `email`  | —        | KEYS_ONLY  | Email-code login (`requestAuthCode`/`verifyAuthCode`): resolve email → user id |

`KEYS_ONLY` for the same reason as `instance.slug-index` — the login path only needs the id to
drive the next `GetItem`.

**Invariant: `email` is always trimmed and lowercase.** Every entry point that writes or looks up
a user email (`createUser`/`updateUser`, `requestAuthCode`/`verifyAuthCode`, the CLI's `user create`
and `--user`, dev auth) runs it through `db::normalize_user_email` first, so `Bob@Example.com` and
`bob@example.com` are the same user and collide on the taken-email pre-check. `email-index` itself
matches exactly — `get_user_id_by_email` must be given a normalized address. No migration shipped
with this: no stored email contained uppercase when it was introduced.

**Non-obvious attributes:**

- `name` (S)
- `enabled` (Bool)
- `created_at` (N) — Unix timestamp
- `access_time` (N) — Unix timestamp of the user's last authenticated request;
  absent until their first one. Throttled to at most one write per minute (see
  `auth::fetch_update_user_auth_info`), so it does not track requests precisely.
- `superuser` (Bool) — admin access to every instance's settings/membership/
  inbound addresses (the `Superuser`/`InstanceOwnerOrSuperuser` GraphQL
  guards) and **nothing else**: a superuser does not pass `Member`/
  `InstanceOwner` and has no implicit ticket access — see `CLAUDE.md`'s
  superuser boundary house rule. Same omit-optional-attributes convention as
  `instance.deleted`: only ever written `true`; absent means `false`.
  Grantable only via `bin/cli.rs`'s `user set-superuser` — no GraphQL
  mutation can set it.

---

### `{prefix}_membership`

| Attribute     | Type | Role                  |
| ------------- | ---- | --------------------- |
| `id`          | S    | Hash key (PK) — nanoid |
| `instance_id` | S    | GSI hash key           |
| `user_id`     | S    | GSI hash key           |

**GSIs:**

| GSI                 | Hash key      | Sort key | Projection | Purpose                                                        |
| ------------------- | ------------- | -------- | ---------- | ---------------------------------------------------------------- |
| `instance_id-index` | `instance_id` | —        | ALL        | Instance settings page: list every member (and role) of an instance |
| `user_id-index`     | `user_id`     | —        | ALL        | `me` query / instance switcher: list every instance a user belongs to |

`ALL` on both: a user typically belongs to a handful of instances, and an instance to a handful
of members, so rendering either list directly from the GSI beats an N-way `BatchGetItem`.

**Non-obvious attributes:**

- `role` (S) — `owner` \| `agent`
- `notify_new_ticket`, `notify_assigned_to_me`, `notify_assigned_to_me_updated`,
  `notify_unassigned_updated`, `notify_assigned_to_others_updated` (all Bool, all optional) — this
  member's `api/src/staff_notify.rs` email-notification preferences. Per the omit-optional-attributes
  house rule, an absent attribute means "use the default" (see `db::NotificationSettings`'s doc
  comment for what each defaults to), never `Bool(false)`; `updateNotificationSettings` `SET`s an
  attribute explicitly on either `true` or `false` and never `REMOVE`s one back to its default.
  Removing and re-adding a membership drops the row — and every attribute on it — so a re-added
  member starts back at the defaults.

---

### `{prefix}_ticket`

| Attribute            | Type | Role                                                                                   |
| -------------------- | ---- | --------------------------------------------------------------------------------------- |
| `id`                 | S    | Hash key (PK) — nanoid                                                                   |
| `instance_status`    | S    | GSI hash key — `"{instance_id}#open"` \| `"{instance_id}#closed"` \| `"{instance_id}#deleted"`, always present |
| `instance_visible`   | S    | Sparse GSI hash key — `"{instance_id}"`, present only when status != `deleted`          |
| `instance_assignee`  | S    | Sparse GSI hash key — `"{instance_id}#{assignee_user_id}"`, present only when assigned and visible |
| `last_activity_at`   | N    | GSI sort key (all three listing GSIs)                                                    |
| `instance_number`    | S    | GSI hash key — `"{instance_id}#{number}"`, always present                                |

**GSIs:**

| GSI                                          | Hash key             | Sort key            | Projection | Purpose                                                     |
| --------------------------------------------- | --------------------- | -------------------- | ---------- | -------------------------------------------------------------- |
| `instance_status-last_activity_at-index`      | `instance_status`     | `last_activity_at`   | ALL        | Open and Closed list pages, newest activity first             |
| `instance_visible-last_activity_at-index`     | `instance_visible`    | `last_activity_at`   | ALL        | All list page (every non-deleted ticket)                      |
| `instance_assignee-last_activity_at-index`    | `instance_assignee`   | `last_activity_at`   | ALL        | "Assigned to me" filter                                        |
| `instance_number-index`                       | `instance_number`     | —                    | KEYS_ONLY  | Subject-tag resolution (`[#{slug}-{number}]`) during inbound-mail threading |

**The three composite marker attributes — `instance_status`, `instance_visible`,
`instance_assignee` — are the whole listing story**, and per the house rule (omit, never
`Null`) are written and `REMOVE`d, never nulled. DynamoDB only indexes an item into a GSI when
the GSI's hash-key attribute is *present* on that item; writing an explicit `Null` would leave the
item indexed (with a null sort position) rather than dropping it out. Deleting a ticket removes
`instance_visible` and `instance_assignee` and flips `instance_status` to `…#deleted` — one write
drops the ticket out of every normal list while it stays queryable by id and by an explicit
`status: DELETED` filter.

`ALL` projection on the three listing GSIs: every ticket list page in the web UI renders
subject/status/requester/assignee/`last_activity_at` straight from the list response, so
projecting everything avoids a per-row `GetItem` for what is, by construction, the common case
(paging through a queue). `KEYS_ONLY` on `instance_number-index`: it is a single exact-match
lookup during inbound routing, and the caller does a follow-up strongly-consistent `GetItem`
before mutating the ticket anyway (GSI reads are only eventually consistent), so nothing is
gained by projecting more.

**Non-obvious attributes:**

- `instance_id` (S)
- `number` (N) — per-instance sequential ticket number; the human-visible part of
  `instance_number` and of the `[#{slug}-{number}]` subject tag
- `subject` (S)
- `status` (S) — `open` \| `closed` \| `deleted`; the un-composited form of `instance_status`'s
  suffix, kept alongside it because resolvers read `status` directly far more often than they
  need the composite
- `requester_emails` (SS) — string set
- `cc_emails` (SS) — string set
- `assignee_user_id` (S) — absent when unassigned
- `reply_token` (S) — opaque 16-char token embedded in `Reply-To` as `+t{reply_token}` for
  threading
- `created_at` (N)
- `updated_at` (N)

---

### `{prefix}_ticket_message`

| Attribute        | Type | Role                  |
| ---------------- | ---- | --------------------- |
| `id`             | S    | Hash key (PK) — nanoid |
| `ticket_id`      | S    | GSI hash key           |
| `created_at`     | N    | GSI sort key           |
| `rfc_message_id` | S    | GSI hash key           |

**GSIs:**

| GSI                             | Hash key         | Sort key     | Projection | Purpose                                                                |
| -------------------------------- | ----------------- | ------------- | ---------- | -------------------------------------------------------------------------- |
| `ticket_id-created_at-index`     | `ticket_id`       | `created_at`  | ALL        | Thread view: every message for a ticket, in order                        |
| `rfc_message_id-index`           | `rfc_message_id`  | —             | KEYS_ONLY  | Inbound-mail threading: resolve `In-Reply-To`/`References` to the message (and ticket) it answers |

`ALL` on `ticket_id-created_at-index`: the thread view renders `body_text`/`body_html`/
`attachments` directly from the list, and a ticket's message count is small enough that
projecting everything beats an N-way `GetItem` per page render. `KEYS_ONLY` on
`rfc_message_id-index`: this is an id-resolution step during inbound routing; the caller reads
the resolved message (and its parent ticket) with a separate strongly-consistent `GetItem` before
appending anything.

**Non-obvious attributes:**

- `kind` (S) — `inbound` \| `reply` \| `note` \| `system`. `note` rows are internal-only and
  filtered out of any requester-visible resolver — they are never emailed
- `author_user_id` (S) — present for `reply`/`note`; absent for `inbound`
- `from_email` (S) — present for `inbound`; absent otherwise
- `to_emails` / `cc_emails` (SS) — snapshot of the envelope at send/receive time, independent of
  the ticket's current requester/CC lists (which can change afterward)
- `body_text` (S), `body_html` (S) — absent if the message had no such part
- `in_reply_to` (S), `references` (S) — absent for the first message on a ticket
- `attachments` (list of `{s3_key, filename, content_type, size}`) — absent when empty
- `raw_s3_key` (S) — present only on `inbound` rows, pointing at the raw MIME in S3

---

### `{prefix}_counter`

| Attribute | Type | Role                                          |
| --------- | ---- | ---------------------------------------------- |
| `id`      | S    | Hash key (PK) — the instance id, not a nanoid  |

No GSIs. One counter row per instance; the row's `id` is the owning instance's id directly (not a
separately generated nanoid), since the counter's whole purpose is a 1:1 relationship with an
instance and there is no other access pattern to support.

The table also holds one **invoice-number reservation row** per finalized invoice:
`id = {instance_id}#invoice#{number}` (`db::invoice_number_reservation_id`; an instance id is a
nanoid, which never contains `#`, so it can't collide with a counter row), with `invoice_id` (S)
and `created_at` (N). It is created by the same `TransactWriteItems` that finalizes the invoice,
conditioned on `attribute_not_exists(id)` — the guarantee that no two invoices in an instance share
a number, now that `finalizeInvoice(number:)` can take one below the counter. Never removed, like
the finalized invoice it records. Invoices finalized before reservation rows existed have none;
`db::Handler::invoice_number_used` also queries the instance's invoices to cover them.

**Non-obvious attributes:**

- `next_ticket_number` (N) — incremented with an atomic `UpdateItem ADD`, never read-then-written;
  see "Known issues and risks" below for what happens if that rule is broken
- `next_invoice_number` (N) — the invoicing counterpart, same atomic-`ADD`-only rule, incremented
  by `finalizeInvoice` (`db::Handler::increment_invoice_counter`) and settable forward-only via
  `setNextInvoiceNumber` (`db::Handler::set_next_invoice_number`, condition
  `attribute_not_exists(next_invoice_number) OR next_invoice_number <= :new_value`). An owner's
  `finalizeInvoice(number:)` (importing an existing invoice under its original number) doesn't
  `ADD`: it moves the counter up to that number through the same forward-only write — a no-op
  for a number at or below it — so automatic numbering continues after the highest number used
  and never lands on a claimed one. Absent means
  `0` (no invoice finalized yet, and `nextInvoiceNumber` has never been set) —
  `InvoicingSettingsInfo.nextInvoiceNumber` reads this lazily (a single `GetItem`, not carried on
  every `Instance` fetch) and reports `this + 1`. See "Known issues and risks" for the same
  gap-not-duplicate reasoning as `next_ticket_number`.

---

### `{prefix}_login_code`

| Attribute | Type | Role                             |
| --------- | ---- | ----------------------------------- |
| `email`   | S    | Hash key (PK) — the address itself |

No GSIs. Ephemeral: `ttl { attribute_name = "expires_at" }`, no deletion protection, no PITR — a
login code is worthless once expired, so there is nothing to protect.

**Exclusively backs `requestAuthCode`/`verifyAuthCode` (authenticated user login).** The public
submit form's email-verification code is a structurally different table (`ephemeral_state`, `kind:
"submit_code"`) — see that table's entry above for why the separation is load-bearing.

**`email` (the hash key) is always trimmed and lowercase**, same invariant and same
`db::normalize_user_email` normalizer as `user.email` above — `requestAuthCode`/`verifyAuthCode`
normalize the `email` argument once, up front, and use that normalized value for every read/write
on this table, so a login code requested as `Bob@Example.com` is found again by
`bob@example.com`.

**Non-obvious attributes:**

- `code_hash` (S) — sha256 of the 6-digit code, never the code itself
- `expires_at` (N) — 10 minutes from issuance; drives DynamoDB TTL deletion
- `attempts` (N) — incremented per failed `verifyAuthCode`; burned after 5
- `last_sent_at` (N) — enforces the one-send-per-30s rate limit

---

### `{prefix}_user_token`

| Attribute    | Type | Role                  |
| ------------ | ---- | --------------------- |
| `id`         | S    | Hash key (PK) — nanoid |

**No GSIs.** The token string carries its own row id (`mtu_{id}.{secret}`), so verification
(`auth::verify_token`'s `mtu_` branch) is a strongly consistent `GetItem` on `id` followed by a
constant-time comparison of the full presented token's sha256 against the stored `token_hash` —
the same shape as `api_token` below, so a just-issued session authenticates on its very first
request with no GSI eventual-consistency window.

**Non-obvious attributes:**

- `user_id` (S)
- `token_hash` (S) — sha256 of the full `mtu_{id}.{secret}` token; the token itself is never stored
- `expires_at` (N) — 8 hours from issuance. Not the table's TTL attribute — `user_token` is a
  durable table with deletion protection + PITR, not one of the three ephemeral tables, so expiry
  is enforced by application-layer comparison against `expires_at`, not by DynamoDB TTL

---

### `{prefix}_oauth_grant`

One row per OAuth client a user has authorized for the MCP interface (`oauth.rs`). A grant is the
unit of revocation: rotating the access or refresh token rewrites the same row.

| Attribute | Type | Role                    |
| --------- | ---- | ----------------------- |
| `id`      | S    | Hash key (PK) — nanoid  |
| `user_id` | S    | GSI hash key            |

**GSIs:**

| GSI             | Hash key  | Sort key | Projection | Purpose                          |
| --------------- | --------- | -------- | ---------- | -------------------------------- |
| `user_id-index` | `user_id` | —        | ALL        | A user's "connected apps" list   |

**Non-obvious attributes:**

- Tokens are `mtoa_{id}.{secret}` (access) and `mtor_{id}.{secret}` (refresh): the grant id is
  embedded, so verification is a strongly consistent `GetItem` by `id` — never a hash GSI — for the
  same reason as `api_token`. Only the sha256 of each full token is stored
  (`access_token_hash`, `refresh_token_hash`).
- `resource` (S) — the audience the tokens are bound to (`<api base>/mcp`), checked on every use
- `client_id`, `client_name`, `redirect_uri`, `scope` (S) — what the user approved
- `access_expires_at` (N) — 1 hour; `refresh_expires_at` (N) — 30 days, sliding on every refresh
  but never past `expires_at`
- `expires_at` (N) — absolute 90-day cap **and** the table's TTL attribute; also checked in
  application code, since TTL deletion can lag
- `last_used_at` (N) — absent until first use; throttled touch

Refresh rotation is a compare-and-swap on `refresh_token_hash`, so two concurrent refreshes with
the same token cannot both win.

---

### `{prefix}_api_token`

Instance-scoped integration credentials, format `mta_{id}.{secret}`, authorising exactly
`submitVerifiedTicket` for `instance_id` — see `auth::AuthInfo::ApiToken`'s doc comment and
CLAUDE.md's "API tokens" house rule.

| Attribute     | Type | Role                  |
| ------------- | ---- | --------------------- |
| `id`          | S    | Hash key (PK) — nanoid |
| `instance_id` | S    | GSI hash key           |

**GSIs:**

| GSI                 | Hash key      | Sort key | Projection | Purpose                              |
| ------------------- | ------------- | -------- | ---------- | ------------------------------------- |
| `instance_id-index` | `instance_id` | —        | ALL        | The token management page's list      |

**No `token_hash` GSI, same as `user_token` above.** The token string carries its own
row id (`mta_{id}.{secret}`, not just an opaque secret), so verification (`auth::verify_token`'s
`mta_` branch) is a `GetItem` on `id` — no GSI, no eventual-consistency window — followed by a
constant-time comparison of the full presented token against the stored `token_hash`. This is the
same trade the `+t{ticket_id}.{reply_token}` reply tag makes, and for the same reason: a token
minted by `createApiToken` must authenticate on its very first use, which a GSI lookup cannot
promise.

**Non-obvious attributes:**

- `name` (S) — an admin-chosen label ("Zendesk sync"), shown on the management page
- `token_hash` (S) — sha256 of the *full* token string (`mta_{id}.{secret}`), never the secret
  alone and never the secret itself, which exists in full only at issuance
- `enabled` (BOOL) — always written (unlike most bool flags in this schema, this is not an
  omit-optional-attributes presence marker); `updateApiToken` flips it, and `verify_token` checks
  it on every request, so disabling takes effect immediately, with nothing cached
- `created_at` (N)
- `created_by_user_id` (S) — the instance owner or superuser who minted it
- `last_used_at` (N) — absent until first use, same throttled-touch convention as
  `user_token.last_used_at`

**No expiry.** Unlike `user_token`/the requester submit token, this is a long-lived integration
credential — an external service configures it once and keeps using it. Revocation is
`updateApiToken(enabled: false)` or `deleteApiToken`, never a clock running out.

---

### `{prefix}_webauthn_credential`

| Attribute | Type | Role                  |
| --------- | ---- | --------------------- |
| `id`      | S    | Hash key (PK) — nanoid |
| `user_id` | S    | GSI hash key           |

**GSIs:**

| GSI               | Hash key  | Sort key | Projection | Purpose                                                          |
| ----------------- | --------- | -------- | ---------- | -------------------------------------------------------------------- |
| `user_id-index`   | `user_id` | —        | ALL        | Passkey login (discoverable-credential flow) and the settings page's passkey list |

`ALL`: both call sites need the full serialized credential (not just its id) — the login flow to
verify a signature, the settings page to render each passkey's metadata.

**Non-obvious attributes:**

- `passkey` (S) — the serialized `webauthn-rs` `Passkey`, opaque to everything except the
  `webauthn-rs` crate
- `name` (S) — user-supplied label, shown in the settings page
- `created_at` (N)
- `last_used_at` (N) — absent until first use

---

### `{prefix}_project`

A client/job an invoicing instance bills against — the first invoicing-only table. See CLAUDE.md's
"Invoicing" house rule. Billable items (below) and invoices (a later PR) each reference a project by id.

| Attribute     | Type | Role                  |
| ------------- | ---- | --------------------- |
| `id`          | S    | Hash key (PK) — nanoid |
| `instance_id` | S    | GSI hash key           |

**GSIs:**

| GSI                 | Hash key      | Sort key | Projection | Purpose                              |
| ------------------- | ------------- | -------- | ---------- | ------------------------------------- |
| `instance_id-index` | `instance_id` | —        | ALL        | The projects list page's data source  |

`ALL`, unpaginated read (`list_projects_by_instance`) — same reasoning as `inbound_address`/
`api_token`'s `instance_id-index`: an instance's clients/jobs are low-cardinality, not a
user-generated table, and every field is rendered directly from the list.

**Non-obvious attributes:**

- `name` (S) — required
- `client_name` (S) — required
- `client_abn` (S) — optional
- `client_address` (S) — optional, multi-line
- `reference` (S) — optional (e.g. a site address distinct from the client's billing address).
  **Note for any future direct `UpdateExpression` on this table:** `reference` is a DynamoDB
  reserved keyword and must be aliased via `ExpressionAttributeNames` (`#ref`) in any update/
  condition/projection expression — a literal `reference = :v` fails with
  `ValidationException: ... reserved keyword: reference`. `update_project` already does this;
  don't regress it.
- `archived` (Bool) — only ever written `true`; absent means active, same omit convention as
  `instance.deleted`/`instance.gst_registered`. There is no delete in v1.
- `created_at`, `updated_at` (N)

Any member (owner or agent) of the invoicing instance can create/update a project — day-to-day
work, not an owner-only setting, unlike the instance's own invoicing settings above. Superusers get
no access (they don't pass `Member`) — the same superuser boundary tickets have always had.

---

### `{prefix}_billable_item`

One line of work recorded against a project, later collected onto an invoice. See CLAUDE.md's
"Invoicing" house rule for the money rules.

| Attribute     | Type | Role                                   |
| ------------- | ---- | -------------------------------------- |
| `id`          | S    | Hash key (PK) — nanoid                 |
| `instance_id` | S    | GSI hash key (denormalised from the project) |
| `project_id`  | S    | GSI hash key                           |
| `date`        | S    | GSI sort key — `YYYY-MM-DD`            |

**GSIs:**

| GSI                      | Hash key      | Sort key | Projection | Purpose                             |
| ------------------------ | ------------- | -------- | ---------- | ----------------------------------- |
| `instance_id-date-index` | `instance_id` | `date`   | ALL        | The instance-wide billable items list |
| `project_id-date-index`  | `project_id`  | `date`   | ALL        | One project's items (project page; the pool an invoice draws from) |

Both are read newest `date` first (`ScanIndexForward = false`), keyset-paginated with a
`{date}:{id}` cursor — shaped like the ticket cursor. The cursor plus the listing's own scope (which
supplies the GSI hash value) is everything an `ExclusiveStartKey` on either index needs: `id`, the
hash attribute, and `date`. Items sharing a date come back in DynamoDB's index order for equal sort
keys (effectively by `id`) — stable across pages, but not creation order.

`ALL` because every field is rendered in the list, and the unbilled/billed filter runs as a
`FilterExpression` over projected rows: `attribute_not_exists(invoice_id)` /
`attribute_exists(invoice_id)`. DynamoDB applies `Limit` **before** the filter, so a filtered page
can come back short or empty while matches remain; `list_billable_items` keeps querying until the
page is full or the index is exhausted, and `billable_items_dynamodb_local.rs` pins that down.

**Non-obvious attributes:**

- `date` (S) — `YYYY-MM-DD`, always the canonical zero-padded form (`invoicing::validate_item_date`
  rejects `2026-8-1`), since the sort key's string order must be the date order. **`date` is a
  DynamoDB reserved word:** any update/condition/filter/key expression must alias it via
  `ExpressionAttributeNames` (`#d`), like `project.reference`. Raw attribute names in an
  `Item`/`Key`/`ExclusiveStartKey` map need no alias.
- `description` (S) — trimmed, non-empty, ≤ 2000 chars, may be multi-line (lines starting `* ` or
  `- ` render as bullets on the invoice)
- `quantity_hundredths` (N) — quantity × 100 (`150` = 1.5), `0 < q ≤ 100,000,000`
- `unit_price_cents` (N) — GST-exclusive, `0 ≤ p ≤ 1,000,000,000`
- `invoice_id` (S) — optional; absent means unbilled. Set by the transactional attach path
  (`createInvoice`/`addInvoiceItems`, `db::Handler`'s `create_invoice`/`add_invoice_items`) and
  cleared by the transactional detach path (`removeInvoiceItems`/`deleteInvoice`), always together
  with the owning `{prefix}_invoice` row's `item_ids` — see that table's entry below. `delete`
  stays conditional on `attribute_not_exists(invoice_id)` regardless of the invoice's own status
  (an item on any invoice, draft or finalized, can't be deleted); `update` is conditional on either
  that (unbilled) or, when the item is on a *draft* invoice, on the invoice still being a draft
  (checked atomically in the same transaction that bumps the invoice's `version` — see
  `db::Handler::update_billable_item`'s doc comment).
- `created_by_user_id` (S), `created_at`, `updated_at` (N)

The line amount is **not stored** — the API derives it (round-half-up of `quantity_hundredths ×
unit_price_cents / 100`) on every read. At the bounds it reaches 10^15 cents, above GraphQL's
32-bit `Int` but exact as a JSON number in JS (under 2^53); it is served as a JSON number.

Any member (owner or agent) of the invoicing instance can create/update/delete items; superusers get
no access.

---

### `{prefix}_invoice`

A project's billable items collected for billing — see CLAUDE.md's "Invoicing" house rule for the
domain rules (snapshot-on-finalize, strict finality, the transactional attach/detach machinery).

| Attribute     | Type | Role                                          |
| ------------- | ---- | ---------------------------------------------- |
| `id`          | S    | Hash key (PK) — nanoid                          |
| `instance_id` | S    | GSI hash key                                    |
| `project_id`  | S    | GSI hash key                                    |
| `created_at`  | N    | GSI sort key (both GSIs)                        |

**GSIs:**

| GSI                          | Hash key      | Sort key     | Projection | Purpose                                   |
| ----------------------------- | ------------- | ------------ | ---------- | ------------------------------------------ |
| `instance_id-created_at-index` | `instance_id` | `created_at` | ALL        | The instance-wide invoices list            |
| `project_id-created_at-index`  | `project_id`  | `created_at` | ALL        | One project's invoices (project page)      |

Both read newest `created_at` first (`ScanIndexForward = false`), keyset-paginated with a
`{created_at}:{id}` cursor — the same shape as `billable_item`'s `{date}:{id}`. `ALL`: the list's
DRAFT/UNPAID/PAID filter (below) runs as a `FilterExpression` over the projected rows, and
`list_invoices` follows `list_billable_items`'s "no `Limit` on a filtered query, keep querying
until the page is full or the index is exhausted" rule for the same reason.

**`status`, `number`, `version`, and `snapshot` are all DynamoDB reserved words** — every
expression that names one aliases it (`#status`, `#num`, `#v`, `#snap` respectively). Raw
attribute names in an `Item`/`Key`/`ExclusiveStartKey` map need no alias, same exception as
`date`/`reference` elsewhere in this schema.

**Non-obvious attributes:**

- `status` (S) — `draft` or `finalized`. **Strictly one-way**: nothing ever writes `finalized` ->
  `draft` — no void, no un-finalize. The only field a finalized invoice's row can still change is
  `paid_date`.
- `version` (N) — optimistic-concurrency counter, starting at 1. Bumped by every write that touches
  `item_ids` (attach/detach) and by editing an item that sits on this (draft) invoice
  (`update_billable_item`'s transactional path) — so a `finalizeInvoice` that read a `version`
  before either kind of change fails its own conditional write cleanly instead of freezing a
  snapshot that's already stale. See "Known issues and risks" for the gap this (deliberately)
  still leaves in the *numbering*, not in `version` itself.
- `item_ids` (SS) — the authoritative set of items on this invoice; absent (empty) is the only way
  a String Set represents "no items". `createInvoice` requires at least one item to start with, but
  `removeInvoiceItems` may empty a draft afterward (finalizing one is refused instead). Every
  `{prefix}_billable_item` row whose `invoice_id` equals this row's `id` must appear here, and vice
  versa — enforced structurally by the transactional writes, never by a separate consistency pass.
- `created_by_user_id` (S), `created_at`, `updated_at` (N)
- `number` (N) — optional; absent for a draft. Assigned exactly once, at finalization, from
  `{prefix}_counter`'s `next_invoice_number` (see that table's entry) — the next in sequence, or
  any unused number an owner gives `finalizeInvoice(number:)`. Unique per instance, enforced by the
  number's reservation row in `{prefix}_counter`. Displayed zero-padded to 3
  digits (`invoicingSettings`'s convention; it simply grows past 999).
- `issue_date` (S) — optional; `YYYY-MM-DD`. Absent for a draft; set once, at finalization, never
  changed afterward.
- `snapshot` (S) — optional; JSON (`invoicing::snapshot::InvoiceSnapshot`, `schema_version: 1`).
  Absent for a draft (whose printable content is instead built live,
  `invoicing::snapshot::build_snapshot`, from the project/instance/items as they currently stand).
  Set once, at finalization, and never touched again — this is the entire mechanism behind "a
  project/settings edit after finalize never changes an already-finalized invoice".
- `total_cents` (N) — optional; absent for a draft. Denormalised from the snapshot (rather than
  parsed out of the JSON on every list read) so the invoices list can show/sort by it cheaply.
- `finalized_at` (N), `finalized_by_user_id` (S) — optional; absent for a draft.
- `paid_date` (S) — optional; `YYYY-MM-DD`. Absent means unpaid. Set/cleared by `setInvoicePaid`
  (condition `status = finalized`); never printed on the invoice itself.
- `pdf_s3_key` (S) — optional; absent until `downloadInvoicePdf` has rendered this invoice's PDF at
  least once. Set exactly once, by `db::Handler::set_invoice_pdf_key` (condition `status =
  finalized`), to `invoices/{instance_id}/{id}/Invoice-{displayNumber}.pdf` — see `storage.rs`'s
  key-layout doc comment. Rendering is deterministic from `snapshot` (content, layout and page
  count — not quite the file's raw bytes; see `api/src/invoicing/pdf.rs`'s doc comment for why),
  so this is an unconditional `SET`, not an `attribute_not_exists` guard: a concurrent
  double-render just writes the same key twice, with two equally correct renderings of the same
  invoice, never a real race. Along with `paid_date`, the only attributes that may still change on
  a finalized invoice.

**Attach/detach/finalize/delete use `TransactWriteItems`** (`Put`/`Update`/`Delete` items only — IAM has no
`TransactWriteItems` action; each item is authorised by the existing `PutItem`/`UpdateItem`/
`DeleteItem` grants, and with no `ConditionCheck` no `dynamodb:ConditionCheckItem` grant is needed)
across this table and
`billable_item` together, via a small `transact_write` helper in `dynamodb.rs` — the first use of
DynamoDB transactions in this codebase. `finalizeInvoice`'s write is a transaction too, but not
with the items — by the time it runs, every item's `invoice_id` already points at this invoice
(from the moment it was attached) — with the number's reservation row in `{prefix}_counter` (see
that table's entry). Both are `Update` items, so `UpdateItem` grants authorise it.

Any member (owner or agent) of the invoicing instance can create/attach/detach/delete a draft, and
finalize or mark paid; superusers get no access — same boundary as `project`/`billable_item`.

---

### `{prefix}_expense`

Money an invoicing instance spent — a purchase, or a cents-per-km vehicle trip — optionally against
one of its projects. See CLAUDE.md's "Expenses" house rule. Never linked to an invoice.

| Attribute     | Type | Role                                   |
| ------------- | ---- | -------------------------------------- |
| `id`          | S    | Hash key (PK) — nanoid                 |
| `instance_id` | S    | GSI hash key                           |
| `project_id`  | S    | GSI hash key — **optional**            |
| `date`        | S    | GSI sort key — `YYYY-MM-DD`            |

**GSIs:**

| GSI                      | Hash key      | Sort key | Projection | Purpose                             |
| ------------------------ | ------------- | -------- | ---------- | ----------------------------------- |
| `instance_id-date-index` | `instance_id` | `date`   | ALL        | The expenses page; the per-person financial-year km total (`date BETWEEN`) |
| `project_id-date-index`  | `project_id`  | `date`   | ALL        | One project's expenses. **Sparse**: an expense with no project has no `project_id` and never appears here |

Read and paginated exactly like `billable_item`'s (newest first, `{date}:{id}` cursor). The
optional category filter is a `FilterExpression`, so `list_expenses` follows the same "keep querying
until the page is full" rule. `sum_vehicle_km_tenths` walks the caller's trips in one financial year
(`instance_id = :i AND #d BETWEEN :from AND :to`, filtered to `category = vehicle_km AND
created_by_user_id = :me`) — bounded by one person's trips in one year.

**Non-obvious attributes:**

- `date` (S) — canonical `YYYY-MM-DD` (`invoicing::validate_item_date`); a reserved word, aliased `#d`.
- `category` (S) — one of a fixed list (`db::ExpenseCategory::as_str`, e.g. `materials`,
  `vehicle_km`). `vehicle_km` decides the row's shape: a trip carries `distance_tenths_km` and
  `rate_cents_per_km` and never `supplier`/`amount_cents`/`gst_cents`; every other category is the
  reverse. `updateExpense` can change category, and `REMOVE`s whichever set no longer applies.
- `supplier` (S) — purchases only; trimmed, ≤ 200 chars.
- `amount_cents` (N) — purchases only; **GST-inclusive** (what was paid), `0 < a ≤ 1,000,000,000`.
- `gst_cents` (N) — optional, purchases only; the GST included in `amount_cents`,
  `0 ≤ g ≤ amount_cents`. Absent means GST-free.
- `distance_tenths_km` (N) — trips only; km × 10, `0 < d ≤ 50,000` (5,000 km).
- `rate_cents_per_km` (N) — trips only; the ATO rate for the trip date's financial year
  (`invoicing::vehicle::ATO_RATES`), looked up on every write and stored, so a later table change
  never alters a saved trip. A trip's amount is **not stored** — derived as round-half-up(`distance
  × rate / 10`) on read.
- `description` (S) — optional for a purchase; required for a trip (its business purpose). ≤ 2000.
- `project_id` (S) — optional; absent (never `Null`) when the expense isn't for a project.
- `created_by_user_id` (S) — whoever logged it; also whose km running total a trip counts toward.
- `created_at`, `updated_at` (N)

Any member (owner or agent) of the invoicing instance can create/update/delete expenses; superusers
get no access. An archived project takes no new expenses, but one already on it stays editable.

---

### `{prefix}_ephemeral_state`

| Attribute | Type | Role                                                                   |
| --------- | ---- | ------------------------------------------------------------------------ |
| `id`      | S    | Hash key (PK) — a random nanoid for WebAuthn challenges; a deterministic, hash-derived string for the two `kind`s below |

No GSIs. Ephemeral: `ttl { attribute_name = "expires_at" }`, no deletion protection, no PITR.
Generic key/value store, namespaced by a `kind` discriminator that is deliberately *not* a GSI
key — every access pattern here is a `GetItem` by `id` (the opaque token/challenge handed to the
client, or a value the caller can recompute), never a scan or query by kind. Backs:

- WebAuthn registration/login challenges (`kind: "reg"` / `"auth"`), `id` a random 32-char nanoid.
- The public submit form's 6-digit email-verification code (`kind: "submit_code"`). **Stored here,
  never in `login_code`** — see `auth::SUBMIT_CODE_STATE_KIND`'s doc comment and
  `tests/submit_code_dynamodb_local.rs` for why that separation is a security requirement (sharing
  storage with the user login-code flow would let a code minted for the public, unauthenticated
  submit form be presented to `verifyAuthCode`, or vice versa). `id` is deterministic —
  `sha256("submit_code_{sha256(instance_id:email)}")`-derived, via `auth::submit_code_state_id` —
  scoped to `(instance_id, email)` rather than `email` alone (unlike `login_code`'s hash key),
  since a requester may hold an outstanding code for more than one public instance at once; a
  second request for the same pair reuses this same slot, which is what makes the 30s resend rate
  limit a single-row read, mirroring `login_code`.
- The requester submit-token flow's short-lived, single-purpose capability token (`kind:
  "submit_token"`), scoped to `{email, instance_id}` and only usable for `submitTicket`. `id` is
  likewise deterministic, derived from the token's own sha256 hash (folded into the row id rather
  than embedding a separate id in the token, since there is no listing access pattern to
  support).

**Non-obvious attributes:**

- `kind` (S) — discriminator; see above
- `payload` (S) — opaque JSON, meaning depends on `kind`
- `expires_at` (N) — drives TTL deletion

---

### `{prefix}_processed_message`

| Attribute        | Type | Role                                         |
| ----------------- | ---- | ----------------------------------------------- |
| `ses_message_id`  | S    | Hash key (PK) — SES's own message id, not a nanoid |

No GSIs. Ephemeral: `ttl { attribute_name = "expires_at" }`, no deletion protection, no PITR.
Inbound-mail idempotency: one row per SES message id, written with a conditional `PutItem`
(`attribute_not_exists(ses_message_id)`) *before* any other processing — a duplicate SQS
delivery of the same message finds the condition already failed and exits cleanly instead of
creating a second ticket or reply. See "Known issues and risks" below for the window this does
and does not close.

**Non-obvious attributes:**

- `processed_at` (N)
- `expires_at` (N) — drives TTL deletion; retained only long enough to outlast SQS's redelivery
  window (the queue's visibility timeout and its 3-retry DLQ policy), not indefinitely

---

## Known issues and risks

> Each entry below that is worth acting on has a GitHub issue: [#4](../../issues/4) message
> ordering within a second, [#9](../../issues/9) inbound mark-before-work, [#10](../../issues/10)
> slug uniqueness, [#12](../../issues/12) ticket-number gaps. This register is the technical
> detail; the issues are where the decision to act gets made. Keep them in step — an entry that
> gets fixed should be struck through here rather than deleted, so the record of what was once
> wrong survives.

This section covers correctness bugs, race conditions, and consistency gaps that follow from the
design above, called out ahead of the code that would trigger them landing (steps 5–7 of the
build plan). Update it as steps land, rather than leaving it purely speculative.

---

### Race conditions

#### `counter` — the per-instance ticket number must come from an atomic `ADD`, never read-then-write

The obvious-looking implementation — `GetItem` the counter row, add 1 in application code,
`PutItem` the result back — has an unavoidable race: two inbound-mail Lambda invocations
processing two new messages for the same instance at (nearly) the same time can both read
`next_ticket_number = 41`, both compute 42, and both write 42. The result is two tickets sharing
one `[#{slug}-42]` subject tag and one `instance_number` GSI value — which breaks
`instance_number-index` lookups (an exact-match query is supposed to resolve to *one* ticket) and
produces a confusing subject collision for the two requesters involved.

The fix is what this table exists for: `UpdateItem` with `ADD next_ticket_number :one` and
`ReturnValues::UpdatedNew`, which DynamoDB executes as a single atomic read-modify-write
server-side — no two concurrent `ADD`s can observe the same "before" value. The counter table has
no other access pattern, so there is no legitimate reason for a resolver to `GetItem` it directly;
if one ever does, that is the bug to look for first.

#### `counter` — finalizing an invoice can leave a gap in the numbering, never a duplicate

`finalizeInvoice`'s algorithm allocates `next_invoice_number` (the same atomic `ADD`, same
gap-not-duplicate guarantee as `next_ticket_number` above) *before* it knows whether the
conditional `UpdateItem` that actually finalizes the invoice will succeed. If that write's
condition fails — the draft's `version` no longer matches, because another edit or another
finalize attempt raced this one and won — the number that was just allocated is never written onto
any invoice: it is permanently skipped, and the caller sees `CONFLICT` ("invoice changed, reload").
An explicit `finalizeInvoice(number:)` has no such gap: its number isn't taken from the counter,
and the reservation row is written in the same transaction as the invoice, so a failed finalize
leaves that number free for a retry.
This is a deliberate trade, not a bug to fix: allocating the number only *after* confirming the
write would succeed would need either a second round trip inside the same logical operation (its
own new race) or folding the counter `ADD` into the same transaction as the invoice write, and
`ADD`ing a shared per-instance counter inside a transaction would serialize every concurrent
finalize across the whole instance on that one row, for a purely cosmetic guarantee (a gap in
`008, 010, 011, …` costs nothing an accountant cares about; a duplicate `010, 010` would). Two
invoices sharing the same `number` is the failure mode this must prevent, and it does: the counter
never moves backward, a failed finalize never writes the number it allocated onto anything, and
every finalize's transaction creates that number's reservation row only if it doesn't already exist
— which is what also covers a number an owner chose explicitly.

#### `processed_message` — the idempotency check has a window, not a guarantee, against non-SQS redelivery

The conditional `PutItem` (`attribute_not_exists(ses_message_id)`) closes the specific race SQS
redelivery creates: two overlapping Lambda invocations for the same `ses_message_id` (an
at-least-once queue redelivering before the first invocation finishes, or two invocations from a
raised `batch_size`) will have exactly one `PutItem` succeed, and the loser exits without touching
`ticket`/`ticket_message`. That is a real, load-bearing guarantee, not a false one.

What it does *not* guard against: the row is written before step 6 ("Store") completes, so a
Lambda that crashes (times out, OOMs, is killed) *after* the `PutItem` succeeds but *before* the
`ticket_message` is stored will leave `processed_message` marked as processed for a message that
was, from the requester's point of view, silently dropped. SQS's retry will not help — the
idempotency check itself will reject the retry. This is the inverse failure mode from the one the
table is designed to prevent, and is an inherent tension in "mark done first, then do the work":
marking done *after* the work closes this gap but reopens the original race (a second delivery
arriving mid-processing would not yet see the row). A durable fix needs a two-phase state (e.g. a
`processing` marker with its own short TTL, promoted to permanent only after the ticket write
commits, with a periodic sweep re-driving anything stuck in `processing` past its TTL) — not
implemented as of this step; flagged here so step 7 (inbound mail) makes a deliberate choice
about it rather than inheriting the simple version by default.

#### Deleted instances — a former member with a ticket id already in hand can still reach it

`User.memberships` filtering out deleted instances (see the `deleted`
attribute's entry above) keeps a former member's instance switcher clean, but
it is a *listing* filter, not an access-control check baked into every
resolver. `Query.ticket(id)`/`Ticket.messages` authorize by checking
`is_member(memberships, ticket.instance_id)` against the caller's *current*
membership list — and that list is computed fresh, from `membership`, on
every request (`auth::fetch_update_user_auth_info`), independent of whether
the instance itself is deleted. So a member of a deleted instance who already
has one of its ticket ids (from an old email thread, a bookmarked link, browser
history) can still open that specific ticket: membership isn't revoked by
deleting the instance, only the instance's own visibility is hidden.

This is deliberate, not an oversight: closing it would mean either stripping
every deleted instance's memberships from `AuthInfo::User` on every request
(a DB read added to the hot path of every authenticated GraphQL call, for a
narrow edge case) or filtering `AuthInfo::User.memberships` at auth time by a
per-instance `deleted` lookup (same cost, same hot path). Both were rejected
for the same reason `access_time`'s throttling exists: `auth::fetch_update_user_auth_info`
runs on every authenticated request, and adding an unconditional extra read
there for a narrow edge case is the wrong trade. If this needs closing later,
the fix is scoped to `Query.ticket`/`Ticket.messages`/`Ticket.*` resolvers
(check the ticket's own instance's `deleted` there, where a read already
happens), not to the auth hot path.

#### `instance.slug` — uniqueness is a pre-check, not a guarantee, against a concurrent pair of creates

DynamoDB only enforces uniqueness on a table's primary key, and `instance`'s primary key is a
random nanoid `id`, not `slug` — so nothing at the database level stops two `PutItem`s from
succeeding with the same `slug`. Two options were on the table:

1. **A deterministic id derived from the slug** (e.g. `id = hash(slug)`), so a conditional
   `PutItem` (`attribute_not_exists(id)`) on the primary key itself enforces uniqueness for real.
   Rejected: `id` is the stable foreign key every other table references (`inbound_address`,
   `membership`, and eventually `ticket`), and slugs are expected to be editable (`instance
   update`/a future settings-page rename). A scheme that ties `id` to the *current* slug breaks the
   moment a slug is renamed — either the id would have to change too (impossible; everything else
   already points at the old one) or the id/slug relationship silently stops being enforced after
   the first rename, which is worse than not enforcing it at all.
2. **An explicit pre-check plus a documented race**: `create_instance`'s callers (`bin/cli.rs`'s
   `instance create`, and, once instance creation is exposed there, the GraphQL layer) call
   `get_instance_id_by_slug` first and reject a taken slug before writing. `db::Handler::create_instance`
   itself does not re-check — a `PutItem` with `condition_expression("attribute_not_exists(id)")`
   only guards against an astronomically unlikely nanoid collision, not a slug collision.

**Chosen: option 2.** The race this leaves open — two concurrent callers that both pass the
pre-check for the same slug, so both writes succeed and two instances end up sharing one slug — is
real but narrow: instance creation is an operator/admin-driven, low-frequency bootstrap action —
`instance create` in the CLI, and, since the superuser feature landed, the equivalent
`createInstance` GraphQL mutation (superuser-only, per `CLAUDE.md`'s superuser boundary) — not a
high-concurrency user-facing path, so the pre-check-then-write window is milliseconds wide and
would require two operators (or one operator and one superuser, racing each other across the CLI
and the web admin UI) creating the exact same organisation at the exact same moment. If it ever
happens, `get_instance_id_by_slug` (`slug-index`, collapsed through `at_most_one`) will start
returning a data-integrity error on the next read — loud, not silent — because the index would
then have two rows for one slug value. The fix at that point is a manual `instance update` to
rename one of the two colliding instances' slugs — CLI-only: `updateInstance` deliberately has no
`slug` argument (slugs are immutable over GraphQL; see that mutation's doc comment) — no automatic
detection or recovery is implemented.

#### `ticket` marker attributes — a crash between two GSI-affecting updates leaves an inconsistent listing state

Setting `instance_status`, `instance_visible` and `instance_assignee` correctly together (e.g. on
delete: flip `instance_status` to `…#deleted`, `REMOVE instance_visible`, `REMOVE
instance_assignee`) needs to happen in a single `UpdateItem` call so it is atomic from any
reader's point of view — DynamoDB does not offer a multi-attribute "all or nothing across two
calls" primitive short of a transaction. As long as every write path that touches more than one
of these three attributes does so in one `UpdateItem`, this is not a real gap (DynamoDB guarantees
atomicity within a single item's `UpdateItem`). It is called out here as a *discipline* risk
rather than a DynamoDB limitation: a future write path that updates `instance_status` in one call
and `instance_visible` in a second, for expedience, would reintroduce exactly this race under a
crash between the two calls. Code review on any new ticket-mutating resolver should check this.

---

### Message thread ordering within one second

`ticket_message.created_at` is in whole seconds, and it is the sort key of
`ticket_id-created_at-index`. Two messages written in the same second — an inbound message and
the notification it triggers is the normal case — therefore tie on the sort key, and DynamoDB may
return tied rows in either order.

`list_ticket_messages` breaks the tie on `id`, so a thread's order is **stable**: a reader never
sees the same thread reshuffle between reads. But `id` is a random nanoid, so within a single
second the order is arbitrary rather than true insertion order. Two agent replies seconds apart
are unaffected; a reply and its automatic notification could display in either order.

**Fix**, when it matters: give `ticket_message` a millisecond-granular sort key (a new attribute,
since `created_at` is second-granular across every other table and is exposed as `createdAt: Int!`
in seconds), or make message ids time-sortable (ULID/KSUID) so the `id` tiebreak *is* insertion
order. Both are schema changes and neither is worth doing before there is a reason.

### GSI eventual consistency

All GSI-based lookups use DynamoDB's default eventually-consistent reads; strong consistency is
not available on GSIs. This is expected to affect, once the relevant steps land:

- **`instance.slug-index`** — an instance created and immediately visited at `/app/:slug` (e.g.
  scripted setup) could momentarily 404.
- **`user.email-index`** — a user created and immediately signing in could momentarily fail to
  resolve. Low risk in practice since account creation and first login are rarely within
  milliseconds of each other.
- **`ticket.instance_number-index`** — a ticket created moments before an inbound reply with a
  matching subject tag arrives could fall through subject-tag resolution to "no match", opening a
  new ticket instead of appending to the existing one. The fallback (new ticket) is not silently
  wrong — the reply is stored, just under the wrong ticket — but it produces a visible duplicate.
- **`ticket_message.rfc_message_id-index`** — the same failure mode as above, for `In-Reply-To`/
  `References` threading of a fast follow-up reply.

Primary-table reads (`GetItem`, `BatchGetItem`) use strongly consistent reads by default, so
entity fetches by id are reliable; only the GSI-mediated resolution steps above carry this risk.
