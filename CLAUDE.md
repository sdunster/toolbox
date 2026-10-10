# CLAUDE.md

Guidance for working on Toolbox. See `README.md` for the product pitch, `DEVELOPMENT.md` for
local setup, and `SCHEMA.md` for the data model.

## House rules (apply from the first commit)

- **DynamoDB: omit optional attributes, never write `Null`.** An absent attribute means "not
  set"; writing an explicit `Null` breaks sparse GSIs (an attribute has to be *absent*, not
  null, for a row to drop out of a GSI that projects it) and complicates hydration. Deleting or
  clearing an optional value means removing the attribute (`REMOVE` in an update expression), not
  setting it to null.
- **One commit per PR, and every commit must be independently deployable.** Squash before
  opening/merging a PR. Don't leave a PR in a half-working state that only becomes correct once a
  later PR lands — see `CONTRIBUTING.md`.
- **`make check` is static-only — it runs no tests.** It covers actionlint, Relay compilation,
  Prettier, ESLint, `tsc`, a production web build, `terraform fmt`/`validate`, `cargo fmt`, the
  GraphQL schema diff, and Clippy. `make test` is the separate target that actually runs the Rust
  and web test suites. Both must pass before a PR is opened.
- **Regenerate `schema.graphql` after any GraphQL change:**
  ```bash
  cd api && cargo run --locked --bin export-schema > schema.graphql
  cd web && npm run relay
  ```
  CI diffs the committed `api/schema.graphql` against a fresh export and fails if they differ.
- **Mutations require `--enable-mutations` on the dev server.** `cargo run --bin poem` (or
  `poem-local`) starts read-only by default; pass `--enable-mutations` to allow writes. This is a
  deliberate guard against accidentally mutating whatever `DB_PREFIX` you're pointed at.
- **No queue abstraction in `api/src/app.rs`.** seslogin has `HasQueues`/`queue.rs`/`sqs.rs`/
  `mockqueue.rs` because its API *produces* to SQS (member sync, NITC export, healthchecks).
  Toolbox's API never produces to SQS — the only queue in this system carries inbound mail,
  and that queue is *consumed* by the inbound-mail Lambda (step 7), a separate binary with no
  GraphQL surface. Don't add a queue trait to `app.rs`/`MyApp` unless the API itself starts
  producing to a queue.
- **Outbound mail: which ticket updates email.** The build plan says "every ticket update that is
  not an internal note mails requesters + CCs" — read literally that would include assignment and
  requester/CC-list edits, which would mean a customer gets an email every time a ticket changes
  hands internally. That's noise, not signal, so the actual rule implemented (`graphql/mutations.rs`)
  is narrower:
  - `replyToTicket` sends the reply itself (`kind: REPLY`) — the agent's own words, always.
  - `submitTicket` sends the requester a brief acknowledgement (`kind: SYSTEM`), so the public
    form's result lands in their inbox with the `Reply-To` thread already wired up.
    `submitVerifiedTicket` sends the same acknowledgement to every `to`/`cc` address — see the "API
    tokens" entry below.
  - `setTicketStatus` sends requesters/CCs a brief notice (`kind: SYSTEM`) **only on a close or a
    reopen** (including restoring a deleted ticket back to open) — see `outbound::status_notice_body`.
    Transitioning *into* `DELETED`, in either direction, sends nothing: deleting a ticket is admin
    housekeeping (spam cleanup, a mistaken submission), not a resolution the customer is owed a
    notification about.
  - `assignTicket`, `addTicketRequester`/`removeTicketRequester`, `addTicketCc`/`removeTicketCc`
    send no **customer** mail at all — which agent owns a ticket, and who else is copied on it, is
    internal bookkeeping. `addInternalNote` never sends customer mail, as the build plan says
    explicitly. (`assignTicket` *does* send a best-effort **staff** notice — see the next entry;
    the requester/CC mutations never send anything, staff included.)
  - A send failure is handled differently depending on whether the mail *is* the mutation's
    primary effect or a secondary side effect of one: `replyToTicket` fails the mutation (the
    message row survives, but the caller is told delivery didn't happen); `submitTicket`'s
    acknowledgement and `setTicketStatus`'s notice are best-effort — logged on failure, never
    surfacing as a mutation error, since the ticket already exists / the status already changed by
    the time mail is attempted. See `reply_to_ticket`'s doc comment in `graphql/mutations.rs` for
    the full reasoning.

- **Staff notifications: a second, separate mail pipeline (`api/src/staff_notify.rs`).** The
  customer-mail rule above is about requesters/CCs. Members/agents get their own opt-out email
  notifications about ticket activity in their instance, stored as five optional `Bool` attributes
  on the `membership` row (absent means the default below, per the omit-optional-attributes house
  rule; `updateNotificationSettings` writes an explicit `true`/`false` once changed; removing and
  re-adding a membership resets to defaults):

  | Setting | Attribute | Default | Meaning |
  |---|---|---|---|
  | `newTicket` | `notify_new_ticket` | `true` | A new ticket is opened (inbound email or the public submit form) |
  | `assignedToMe` | `notify_assigned_to_me` | `true` | Someone else assigns a ticket to this member |
  | `assignedToMeUpdated` | `notify_assigned_to_me_updated` | `true` | A ticket assigned to this member gets an update (also covers being unassigned/reassigned away — see below) |
  | `unassignedUpdated` | `notify_unassigned_updated` | `true` | An unassigned ticket gets an update |
  | `assignedToOthersUpdated` | `notify_assigned_to_others_updated` | `false` | A ticket assigned to someone else gets an update — off by default; opting into every other agent's traffic is a deliberate choice |

  "An update" is a customer message (inbound mail on an existing ticket), an agent reply
  (`replyToTicket`, only after the customer-facing send succeeds), an internal note
  (`addInternalNote`), or a status change (`setTicketStatus`) — **except a transition *into*
  `DELETED`**, mirroring the customer-mail carve-out above. Assignment (`assignTicket`) is its own
  event: it mails only the new assignee (`assignedToMe`) and the previous assignee
  (`assignedToMeUpdated`, told they were unassigned or the ticket went to someone else) — it never
  fans out to the rest of the team, and a self-assign or a no-op reassignment notifies nobody. The
  actor who performed the action is always excluded, superusers get nothing by virtue of being
  superusers (only a real membership row counts), and a deleted ticket notifies nobody regardless
  of event or settings. See `staff_notify::select_recipients`'s doc comment for the exact rules,
  unit-tested there.

  A staff notice is deliberately built differently from customer mail (`staff_notify::build_staff_mime`,
  not `outbound::build_outbound`): `From` the system sender (`mail::system_from()`) under the
  instance's display name, `Reply-To` the system reply-to, **no `+t{ticket_id}.{reply_token}` tag
  anywhere**, one message per recipient (`To:` only, no `Cc:`), a subject that does not parse as
  `[#{slug}-{number}]` (`outbound::parse_subject_tag` must return `None` for it — pinned by a unit
  test), and never persisted as a `ticket_message` row. The reason is structural, not cosmetic: SES
  receives every address on the support domain, so a staff member hitting "reply" on a notice that
  *did* carry a reply tag would have their reply mistaken for a customer message by the inbound
  pipeline. Like `submitTicket`'s acknowledgement and `setTicketStatus`'s notice, sending is
  best-effort — every failure is `warn!`-logged and swallowed, never surfacing as a mutation error
  or failing inbound processing.

  `updateNotificationSettings` (GraphQL) is the only way to change these, and only for the caller's
  *own* membership — resolved via `list_memberships_by_user(caller)` filtered to the given
  instance, never a trusted membership id. `MembershipInfo.notificationSettings` is **self-only**:
  `FORBIDDEN` for anyone reading someone else's `memberships` (e.g. a superuser via `adminUser`),
  the same defence-in-depth posture as `User.passkeys`.

  Needs `APP_BASE_URL` (the web app's origin, for the "View ticket"/"Change your notification
  settings" links a notice carries) set in every environment that sends this mail — `api/src/staff_notify.rs`
  defaults to `http://localhost:5173` otherwise. The inbound-mail Lambda also needs `MAIL_FROM` now
  that it sends mail of its own (previously it only consumed the inbound queue) — without it, a
  staff notification sent from that Lambda falls back to `mail::FROM_FALLBACK`'s reserved `.test`
  domain and is refused by the provider.

- **Rows reference an instance by `id`, never by `slug`.** Slugs are editable and exist for
  humans; `id` is the foreign key every other table stores. Anything taking an instance from user
  input (the CLI's `--instance`, an argument, a path segment) must resolve it through
  `db::resolve_instance_id`, which accepts either and always returns the id. Skipping that step
  does not fail anywhere: the row is written, listings that look up by the same wrong string find
  it again, and mail still routes — it only surfaces later as an owner whose membership exists but
  who sees no instances, because a resolver looked the instance up by id and found nothing. There
  is a regression test in `tests/inbound_routing_dynamodb_local.rs`.

- **Two senders, deliberately.** System mail (login codes — anything not scoped to an instance)
  sends from `MAIL_FROM`. Instance mail (replies, notifications) sends from that instance's own
  primary inbound address, which is what makes a reply thread back to the right tenant. The
  `FROM_FALLBACK` constant is on a reserved `.test` domain on purpose: a deployment that forgets to
  set `MAIL_FROM` should be refused by the provider rather than quietly send from a domain someone
  else owns.

- **The reply tag carries both ids: `+t{ticket_id}.{reply_token}`.** Not the token alone. The
  token has no index, so resolving by it would need a GSI — and a GSI is eventually consistent, so
  an autoresponder arriving a second after a ticket is created would miss it and open a duplicate.
  Ticket id first makes it a strongly consistent `GetItem`; the token is then compared in constant
  time, and is what stops someone emailing into an arbitrary ticket by guessing a short id.
  `outbound.rs` generates this address and `inbound/routing.rs` parses it — a test asserts the
  round trip, and it is the contract between the two halves of the mail pipeline.

- **Superuser boundary: admin + instance settings, never ticket access; grantable only via the
  CLI.** `db::User::superuser` gates the `Superuser`/`InstanceOwnerOrSuperuser` GraphQL guards —
  instance/user/membership/inbound-address management (`createInstance`, `createUser`,
  `addMember`, `addInboundAddress` via `InstanceOwnerOrSuperuser`, and friends) — and nothing else.
  A superuser does **not** pass `Member`/`InstanceOwner`, has no implicit access to any instance's
  tickets, and does not appear in `User.memberships`/the instance switcher unless they hold a real
  membership row. No GraphQL mutation can set `superuser`; the only way to grant or revoke it is
  `bin/cli.rs`'s `user set-superuser`. Don't widen this — a future "superuser can see all tickets"
  feature needs its own explicit guard, not a loosening of `Superuser`/`InstanceOwnerOrSuperuser`.

- **Members see each other; nobody sees a colleague's other instances.** `Instance.members` is
  `MemberOrSuperuser` — any agent, not just an owner, can list the team so tickets can be handed
  between colleagues (issue #2: names/emails within an instance aren't sensitive). The flip side is
  that `User.memberships` is **self-or-superuser** (`FORBIDDEN` otherwise): a colleague's `User`,
  reached via `members` or `Ticket.assignee`, must not reveal which other tenants they belong to.

- **API tokens (`mta_`) authorise `submitVerifiedTicket` and nothing else.** `AuthInfo::ApiToken`
  is a third principal, instance-scoped, minted only by that instance's owner or a superuser
  (`createApiToken`, guarded `InstanceOwnerOrSuperuser` like `addInboundAddress` — an integration
  credential is instance settings, not something a plain agent hands out). It never passes
  `Authenticated` or any other guard — including `Superuser`/`InstanceOwnerOrSuperuser` themselves —
  so a leaked token reaches exactly one mutation, never `createAttachmentUpload`, never `me`, never
  the token-management mutations that could mint or revoke more of itself.
  - **Id embedded in the token, no `token_hash` GSI** — same shape, same reasoning, as the
    `+t{ticket_id}.{reply_token}` reply tag two entries below: `mta_{id}.{secret}`, verified by a
    `GetItem` on `id` (no GSI, no eventual-consistency window) followed by a constant-time compare
    of the full token against the stored `token_hash`. A token must authenticate on its very first
    use, which a GSI lookup cannot promise. `mtu_` session tokens use the same `mtu_{id}.{secret}` shape
    (`auth::parse_id_token`), for the same reason.
  - `submitVerifiedTicket(subject, body, to, cc)` takes the instance from the token, never an
    argument, and takes `to`/`cc` **on the caller's word** — no email-verification code, unlike the
    public submit form's `Requester` token. That trust is the entire point of an owner/superuser-
    minted credential: an external service that has already verified its own users' addresses (a
    customer portal behind its own login) opens a ticket with the right requester/CC list in one
    call. `to`/`cc` are normalized, deduped, and capped at 20 combined recipients — a leaked token
    must not become a bulk-mail relay — and any address matching one of the instance's own inbound
    addresses is rejected outright, not silently dropped (dropping it could leave a ticket whose
    requester is itself).
  - No expiry: a long-lived integration credential, not a session. Revocation is
    `updateApiToken(enabled: false)` or `deleteApiToken`.

- **Invoicing: a second, separate function from support, living in its own instances.**
  `db::InstanceKind` (`Support`/`Invoicing`) is set once at `createInstance`/`instance create
  --kind` time and is **immutable after creation** — no mutation, and no CLI command, changes it.
  Per the omit-optional-attributes house rule, `kind` is written to the row only for `Invoicing`;
  every instance created before invoicing existed is implicitly `Support`, no migration needed.
  Invoicing instances reuse the existing instance/membership machinery (same instance switcher,
  same `Member`/`InstanceOwner` roles) — there is no separate tenant or membership concept.
  - **Kind isolation, enforced structurally, not just in the web UI.** A support-only operation
    (`addInboundAddress`, `createApiToken`, `submitTicket`/`submitVerifiedTicket`, the requester
    submit-code flow, `publicInstances`/the `/submit` list, and the inbound-mail pipeline's
    instance resolution) rejects an invoicing instance **identically to how it already treats a
    missing or deleted one** — `NOT_FOUND`/dropped mail/excluded from a list, never a distinct
    "wrong kind" error, so none of these can be used to probe an instance's kind. An
    invoicing-only operation (projects, `updateInvoicingSettings`) rejects a support instance with
    a plain `anyhow!` validation error instead — the caller is a real member of a real instance,
    just the wrong kind for what they're asking to do. Both directions go through the one shared
    `db::require_instance_kind` helper, so a future invoicing operation can't independently drift
    on the check.
  - **Invoicing settings are instance settings, not ticket-adjacent data.** The seller
    details/payment footer an invoice prints (`business_name`, `business_abn`, `business_address`,
    `business_phone`, `business_email`, `payment_details`, `gst_registered`, `currency`) live as
    optional attributes on the `instance` row itself, guarded `InstanceOwnerOrSuperuser` to write
    (`updateInvoicingSettings`) — the same posture as `addInboundAddress`/`createApiToken` — but
    readable by any member, like every other plain `Instance` field. `updateInvoicingSettings` is a
    **full replace**: every call writes every field, and a blank string `REMOVE`s the
    corresponding attribute rather than storing an empty one, per the omit-optional-attributes
    house rule. `gst_registered` follows `Instance::deleted`'s convention: only ever written
    `true`; absent means not registered. `currency` absent means `"AUD"`
    (`db::Instance::currency_or_default`); a non-blank value must pass `db::validate_currency_code`
    (3 uppercase ASCII letters).
  - **Projects (`{prefix}_project`) are the first invoicing-only table.** A project belongs to one
    invoicing instance and holds a client/job's billing identity (name, client name, optional ABN/
    address/reference) plus an `archived` flag (same only-ever-`true` convention as
    `Instance::deleted`/`gst_registered` — there is no delete in v1). Any member — owner or agent —
    can create/update a project; this is day-to-day work, not an owner-only setting, unlike
    invoicing settings above.
  - **Billable items (`{prefix}_billable_item`): money is integers, never floats.** Quantity is a
    decimal with ≤ 2 dp, `0 < q ≤ 1,000,000`, stored as integer hundredths
    (`quantity_hundredths`) and exchanged over GraphQL as a **string** (`"1.5"`) that only the
    server parses (`invoicing::money::parse_quantity`; the web's `lib/money.ts` mirrors it for
    live previews only). Unit price is integer cents, GST-exclusive, `0 ≤ p ≤ 1,000,000,000`. The
    line amount is never stored: `amountCents` = round-half-up(`quantity_hundredths ×
    unit_price_cents / 100`) (`invoicing::money::line_amount_cents`), so it can't drift from its
    inputs. An item is authorised through its project (`createBillableItem`) or its own
    `instance_id` (id-only update/delete) — `NOT_FOUND` for missing and not-yours alike, like
    `updateProject`. An archived project takes no new items. `invoice_id` (absent = unbilled) is
    the item↔invoice link — `delete` always refuses an item that has one, `CONFLICT`, both up front
    and in the write's condition expression; `update` narrows that to "refuses an item on a
    *finalized* invoice" — an item on a *draft* invoice stays editable, in a transaction that also
    bumps the draft's `version` (see the invoices entry below). `date` (`YYYY-MM-DD`, canonical
    form only) is both listing GSIs' sort key **and a DynamoDB reserved word** — alias it (`#d`) in
    any expression.
  - **Invoices (`{prefix}_invoice`): snapshot-on-finalize, strict finality, transactions.**
    `createInvoice(projectId, itemIds)` starts a draft with ≥ 1 unbilled item from that project;
    `addInvoiceItems`/`removeInvoiceItems` attach/detach items on a draft (removing every item —
    an empty draft — is allowed; finalizing one is not); `deleteInvoice` removes a draft outright.
    All four write the invoice's `item_ids` and the affected items' `invoice_id` together in one
    `TransactWriteItems` call (`Update`/`Put`/`Delete` items only — IAM authorises each item by its own `PutItem`/
    `UpdateItem`/`DeleteItem` grant, and there is no `ConditionCheck`, so `infra/iam.tf` needs no
    change; adding one would need `dynamodb:ConditionCheckItem`), conditioned on the
    invoice still being `draft` at the `version` the caller last read — the first use of DynamoDB
    transactions in this codebase (`dynamodb.rs`'s `transact_write` helper). `finalizeInvoice`
    freezes everything the invoice prints (seller/bill-to/reference, sorted lines, totals, GST
    flag, currency, payment text, number, issue date) into a JSON `snapshot`
    (`invoicing::snapshot::build_snapshot`, `schema_version: 1`) via one conditional `UpdateItem`
    (not a transaction — every item's `invoice_id` already points here); later edits to the project
    or instance settings never alter it. **Strictly one-way**: no void, no un-finalize — the only
    mutation a finalized invoice still accepts is `setInvoicePaid` (any member; not printed on the
    invoice). The number comes from `{prefix}_counter`'s `next_invoice_number`, the same atomic-`ADD`
    counter pattern as tickets' `next_ticket_number` — allocated *before* the conditional finalize
    write, so a version mismatch there leaves a **gap**, never a duplicate (see `SCHEMA.md`'s
    "Known issues"). Owner-or-superuser `setNextInvoiceNumber` can move it **forward only**
    (`CONFLICT` otherwise); `finalizeInvoice` additionally requires `businessName` to already be
    set ("Complete the invoicing settings first" otherwise). A draft's live preview and a finalized
    invoice's frozen content are built by the exact same `build_snapshot` function — the web
    preview and the printed invoice can't diverge in how a value is computed, only in *when* the
    inputs were read. `status`, `number`, `version`, and `snapshot` are all DynamoDB reserved
    words — alias every one of them (`#status`/`#num`/`#v`/`#snap`) in any expression.
  - **Superusers get no access to projects, billable items, or invoices — same boundary as
    tickets, unwidened.** A superuser doesn't pass `Member`, so `projects`/`project`/
    `createProject`/`billableItems`/`createBillableItem`/`invoices`/`invoice`/`createInvoice` and
    friends all reject one exactly as they would any other non-member; only a real membership row
    grants access, mirroring the existing "superuser is admin + instance settings, never ticket
    access" boundary this project has had since the superuser feature landed. `setNextInvoiceNumber`
    (like `updateInvoicingSettings`/`createApiToken`) is the one exception, by design —
    `InstanceOwnerOrSuperuser`, not `Member`. Don't add any other superuser carve-out here without a
    matching, explicit reason — see the "Superuser boundary" entry above.
  - **The PDF is rendered from the frozen `snapshot`, nothing else.** `downloadInvoicePdf`
    (`Member`, `NOT_FOUND` posture like every other invoice mutation, `CONFLICT` on a draft — there
    is nothing frozen yet to print) parses `invoice.snapshot` and calls `invoicing::pdf::render_invoice_pdf`,
    a pure function (`api/src/invoicing/pdf.rs`) that reads no project/instance state of its own —
    the same "can't diverge from what was frozen" guarantee finalization gives the web preview
    extends to the PDF. The rendered bytes are cached once, at
    `invoices/{instance_id}/{invoice_id}/Invoice-{displayNumber}.pdf` (`db::Invoice.pdf_s3_key`,
    `db::Handler::set_invoice_pdf_key`, conditioned on `status = finalized`), and every later call
    just presigns that key instead of re-rendering. Rendering is deterministic from the snapshot —
    same content, same layout, same page count, every time — so two callers racing the first render
    just overwrite each other with an equally correct rendering of the same invoice: not a race the
    code needs to guard against. The two renders' *bytes* aren't quite identical, though: printpdf
    0.7 has no public way to make the PDF trailer's `/ID` deterministic (a fresh random id on every
    save, unconditionally — see `api/src/invoicing/pdf.rs`'s doc comment), so that's the one thing
    that differs. The download URL forces
    `Content-Disposition: attachment; filename="Invoice-{displayNumber}.pdf"`
    (`storage::Handler::presign_get_download`, `storage::sanitize_download_filename` keeping only
    `[A-Za-z0-9._-]`, since the filename lands unescaped in an HTTP header). Fonts are Liberation
    Sans Regular/Bold, embedded via `include_bytes!` from `api/assets/fonts/` under the SIL Open
    Font License 1.1 (`api/assets/fonts/OFL.txt`) — chosen because `printpdf` needs a real embedded
    TTF for Unicode text and both the crate and the font are pure Rust/no native deps, so the
    renderer builds standalone for `cargo lambda`.

- **Expenses (`{prefix}_expense`): what an invoicing instance spends, never invoiced.** Optionally
  linked to one of the instance's projects (`project_id` omitted when unset, so the sparse
  `project_id-date-index` drops it). Same guards and kind check as billable items (`Member`,
  `db::require_instance_kind`, superusers get nothing, `NOT_FOUND` for missing and not-yours alike),
  and an archived project takes no *new* expenses. Every expense can be edited or deleted — none is
  ever on an invoice, and v1 deliberately has no rebilling and no receipt uploads.
  - **Categories are a fixed list in code** (`db::ExpenseCategory`/`ExpenseCategoryType`, labels in
    `web/src/lib/expenses.ts`), not per-instance configuration, so totals stay comparable. Adding
    one is adding an enum value in all three places.
  - **A purchase stores its GST-inclusive `amount_cents` plus an optional `gst_cents`** (the GST
    in it; absent = GST-free). The web form pre-fills total ÷ 11 until the user overrides it; the
    API only checks `0 ≤ gst ≤ amount`.
  - **`VEHICLE_KM` is a cents-per-km trip, not a purchase**: `distanceKm` (≤ 1 dp, ≤ 5,000) plus a
    required business-purpose `description`; supplier/amount/GST are refused. The rate is **never
    taken from the caller** — `invoicing::vehicle::ATO_RATES` maps a financial year (1 July start)
    to the ATO's published cents-per-km rate, looked up from the trip's date on every write and
    stored on the row; the amount is derived on read. A date in a financial year the table doesn't
    cover yet is refused, never priced at a guess. **Each July, add the new year's rate** to
    `ATO_RATES` (with its test row) and to the preview mirror in `web/src/lib/expenses.ts`.
  - **The ATO's 5,000 km cap is shown, never enforced**: `vehicleKmSummary` sums the *caller's
    own* trips (`created_by_user_id`) in one financial year — the cap is per person and per car,
    which Toolbox can't fully know — and the web turns amber past 4,500 km and red past 5,000.
  - `invoicing::expense::validate_expense_input` is the one place an `ExpenseFields` is built from
    input, so the category ↔ shape rule can't drift between create and update.

- **OAuth tokens (`mtoa_`/`mtor_`) are for the MCP interface only and never reach GraphQL.**
  `api/src/oauth.rs` holds one `oauth_grant` row per client a user authorizes (access + refresh
  token hashes, audience-bound to `<api base>/mcp`). Like `mta_`, the grant id is embedded in
  the token (`mtoa_{grant_id}.{secret}`) so verification is a strongly consistent `GetItem`.
  `auth::verify_token` deliberately does **not** accept them — a test pins that — and only
  `oauth::verify_access_token` (called from the MCP handler) does, resolving to a normal
  `AuthInfo::User` with `grant_id` set, so an MCP token can't drive the site API outside its own
  tool set. Dynamic client registration is stateless: a `client_id` is the registration JSON plus an
  HMAC signed with a key derived from `OAUTH_CLIENT_ID_SECRET` (Toolbox has no JWT secret to reuse);
  registration is unavailable while that variable is unset.
  - **The HTTP surface** (`api/src/oauth_http.rs`) is framework-agnostic — `(app, key, host, body) →
    HttpReply` — and wired into both `server.rs` (poem) and `bin/lambda/handler.rs` ahead of GraphQL:
    `GET /.well-known/oauth-authorization-server`, `POST /oauth/register`, `POST /oauth/token`.
    The consent page is a **web-app** route (`/app/oauth/authorize`), not an API one, so CloudFront
    sends `/oauth/*` to the API but must never send `/oauth/authorize` anywhere else — keep it out of
    that path space. `oauthAuthorizationRequest`/`approveOauthAuthorization` need a real `User`
    session (`mtu_`), never a `Requester`/`ApiToken`, and an `mtoa_` token can't reach GraphQL, so an
    MCP client can't approve further grants for itself. An authorization code is single-use: it is
    deleted from `ephemeral_state` *before* anything else is validated, so a replay fails even with
    the right PKCE verifier. Presenting an already-rotated refresh token revokes the whole grant
    (OAuth 2.1 reuse detection).
  - **`API_BASE_URL` is required in every deployment behind CloudFront** (set in `lambda_api.tf`):
    CloudFront doesn't forward `Host`, so the `Host`-derived fallback would advertise the Function
    URL's origin as the issuer and clients would reject it. `APP_BASE_URL` is where the consent
    page lives.
  - **"Connected AI apps" is self-only, like passkeys.** `User.oauthGrants` and `revokeOauthGrant`
    (Settings page → "Connected AI apps") are `FORBIDDEN`/not-found for anyone but the grant's own
    user — **superusers included**, unlike seslogin. Which AI clients a member has authorized is
    theirs to see, and a superuser who can't list another user's grants has no use for a blind
    revoke either; disabling a user already stops every credential of theirs (verification runs
    `fetch_update_user_auth_info`), and re-enabling restores access. Revoking someone else's grant
    fails with the *same* "not found" as a nonexistent id, so ids can't be probed. Token hashes and
    the client id are never exposed — the redirect host is what identifies a client to a viewer.

- **MCP interface (`api/src/mcp/`): `POST /mcp`, stateless JSON-RPC, tools that only ever run GraphQL.**
  A hand-rolled Streamable-HTTP dispatcher (no `rmcp`: its session machinery has to be switched off
  to suit Lambda, and our bearer check is an async DB call that must precede its dispatch anyway).
  Auth is `oauth::verify_access_token` only (`mtoa_`, audience `<api base>/mcp`); a missing or bad
  token gets a 401 with the RFC 9728 `WWW-Authenticate` challenge. **No tool touches the database.**
  Each runs a fixed GraphQL document through `mcp::tool::ToolContext::run` with the caller's own
  `AuthInfo`, so every existing guard applies unchanged — including the superuser boundary: a
  superuser without a membership sees no instance, ticket or invoice through MCP either. A tool can
  narrow what its caller could do over GraphQL, never widen it. A family of tools is one module
  exposing `catalogue()` and `dispatch()`; register it in `mcp::tool_catalogue`/`dispatch_tool`.
  The ticket tools (`mcp/tickets.rs`) send customer mail exactly as the same GraphQL mutation does —
  see the "Outbound mail" rule above — and say so in their descriptions (`reply_to_ticket` always,
  `set_ticket_status` on a close or reopen). Attachments are deliberately not exposed.
  The invoicing tools (`mcp/invoicing.rs`) send no email and are refused for a support instance by
  the resolvers' own kind check. `finalize_invoice` is strictly irreversible, so its description
  says so, it is annotated destructive, and it requires an explicit `issueDate` (no default); the
  owner-level `updateInvoicingSettings`/`setNextInvoiceNumber` are deliberately not tools.
  Every tool's description must say plainly if it sends customer email. `whoami` (memberships with
  role and instance kind) is the first tool; call it first to learn which instances apply.

- **Tests that touch environment variables must serialize on a `tokio::sync::Mutex` held across
  every `.await`.** The process environment is global and tests run in parallel, so a test that
  sets a var, releases its lock, and only then awaits leaves a window for another test to change
  it underneath. That failure is invisible locally and shows up in CI. `clippy::await_holding_lock`
  objects to holding a *std* guard across an await — the answer is a tokio mutex, not a shorter
  critical section. See `mail::OVERRIDE_TO_ENV_LOCK` and `turnstile`'s `ENV_LOCK`.

## Deployment notes

- **DNS is not managed by Terraform.** The parent zone lives in a different AWS account, so
  `infra/dns.tf` computes every record that must exist and exposes them as the
  `dns_records_required` output for manual creation. Consequence: the first apply blocks on
  `aws_acm_certificate_validation` waiting for a record it cannot create. That is a documented
  two-phase sequence, not a hang — see `DEVELOPMENT.md` §9.
- **Terraform owns the Lambda functions; CI owns their code.** Each function points at
  `placeholder.zip` with `ignore_changes = [filename, source_code_hash]`, so an apply never fights
  a deploy and the deploy role needs neither `iam:PassRole` nor `lambda:CreateFunction`.
- **The GitHub OIDC subject may be immutable.** GitHub can issue
  `repo:owner@<owner_id>/name@<repo_id>:ref:...` instead of `repo:owner/name:ref:...`, and which
  you get is a repository setting. The trust policy accepts both. If a deploy ever fails with
  `Not authorized to perform sts:AssumeRoleWithWebIdentity` while the policy looks correct, read
  the actual `sub` out of the CloudTrail `AssumeRoleWithWebIdentity` event rather than guessing —
  that is how this was found. Never widen it to a `StringLike` wildcard to make a deploy pass.
- **The Terraform state bucket is created by hand**, not by Terraform — it has to exist before
  `init` can store state in it. `DEVELOPMENT.md` §9.2 has the commands.
- **`make check` validates Terraform under a throwaway `TF_DATA_DIR`.** Once an operator has run a
  real `init` against the S3 backend, a plain `init -backend=false` does not undo it and `validate`
  starts demanding credentials it has no business needing for a static check.

## Scope note

The build plan's numbered steps (scaffold → API foundations → auth → instances/memberships →
tickets → outbound mail → inbound mail → web → infra/deploy) are all complete and deployed.
`SCHEMA.md`'s "Known issues and risks" register is the live list of things known to be imperfect;
open GitHub issues track the rest. Check what is already in the tree before assuming something
does not exist.
