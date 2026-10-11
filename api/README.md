# GraphQL API server for Toolbox

> **First-time setup is in [../DEVELOPMENT.md](../DEVELOPMENT.md)** — toolchain, environment,
> and running the full stack with `make dev` / `make dev-local`. This file will grow
> API-specific details (dev server flags, CLI usage, local mail fixtures) as those land.

Prerequisites: Rust via [rustup](https://rustup.rs) — the exact version is pinned in
`rust-toolchain.toml`.

```
cargo test
cargo run --locked --bin export-schema > schema.graphql
```

## OAuth authorization server (for the MCP interface)

`api/src/oauth_http.rs` serves an OAuth 2.1 authorization server so an AI client can act as a
signed-in member, with exactly that member's permissions. All routes sit outside GraphQL and are
served by both `poem` and the Lambda handler:

| Route | What |
|---|---|
| `GET /.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `POST /oauth/register` | RFC 7591 dynamic client registration — public clients only, nothing stored (the `client_id` is the registration JSON + an HMAC) |
| `POST /oauth/token` | `authorization_code` (PKCE S256, single-use code) and `refresh_token` (rotation, with reuse detection) grants |

The consent step is the web page at `/app/oauth/authorize`, backed by the GraphQL query
`oauthAuthorizationRequest` and mutation `approveOauthAuthorization` (both need a signed-in user
session). Tokens are `mtoa_…`/`mtor_…` and are **not** accepted by `/graphql`.

Configuration: `OAUTH_CLIENT_ID_SECRET` (signs client ids; registration answers `503` without it),
`API_BASE_URL` (the OAuth issuer — **required behind CloudFront**, which doesn't forward `Host`)
and `APP_BASE_URL` (where the consent page lives). `local/local.env` sets all three for local dev.

## MCP interface

`POST /mcp` (`api/src/mcp/`) is a Model Context Protocol server (Streamable HTTP, stateless, plain
JSON) so an AI client can work in Toolbox as a signed-in member. It is authenticated only by the
OAuth `mtoa_` access tokens above, and every tool runs a fixed GraphQL document as that member, so
it has exactly their permissions and no more. `GET`/`DELETE /mcp` answer `405`;
`/.well-known/oauth-protected-resource[/mcp]` (RFC 9728) points clients at the authorization server.

Tools:

| Tool | What | Customer email? |
|---|---|---|
| `whoami` | The caller's identity and every instance they belong to, with role and kind | — |
| `list_tickets` | Tickets in a support instance (status, `assignedToMe`/`assignedTo`, cursor paging) | — |
| `get_ticket` | One ticket with its whole conversation (customer mail, replies, internal notes) | — |
| `reply_to_ticket` | Reply to the requesters and CCs | **yes** |
| `add_internal_note` | Note visible to members only | no |
| `set_ticket_status` | Close / reopen / delete | **on close and reopen** |
| `assign_ticket` | Assign to a member, or unassign | no |
| `update_ticket_recipients` | Add/remove one requester or CC | no |
| `list_instance_members` | Members of an instance the caller owns (to find an id to assign to) | — |

Invoicing tools (INVOICING instances only; only `send_invoice`/`send_credit_note` send email):

| Tool | What |
|---|---|
| `list_projects` / `create_project` / `update_project` | Client/job billing identity; `update_project` merges (empty string clears a field) and can archive |
| `list_billable_items` / `create_billable_item` / `update_billable_item` / `delete_billable_item` | Work to be invoiced. Quantity is a decimal string (≤ 2 dp), price is GST-exclusive integer cents; the amount is computed server-side |
| `list_invoices` / `get_invoice` | Summaries, and the full invoice (lines, seller, bill-to, items) |
| `create_invoice` / `add_invoice_items` / `remove_invoice_items` / `delete_invoice` | Draft invoices |
| `finalize_invoice` | **Irreversible**: numbers and freezes the invoice; requires an explicit `issueDate`; `dueDate` defaults from payment terms |
| `set_invoice_paid` / `get_invoice_pdf_url` | Pay the whole balance on a date (omit to remove every payment); presigned PDF link for a finalized invoice |
| `record_invoice_payment` / `delete_invoice_payment` | Part-payments; the invoice is PAID once the balance reaches zero |
| `send_invoice` / `send_credit_note` | **Sends email** to the client with the PDF attached |
| `issue_credit_note` / `list_credit_notes` / `get_credit_note_pdf_url` | **Irreversible** credit (adjustment) notes against a finalized invoice |
| `rebill_expense` | Turn a project expense into a billable item, optionally marked up |
| `get_gst_report` / `get_receivables` / `export_csv` | BAS figures (cash or accrual), aged receivables, accountant CSVs |
| `list_expenses` / `create_expense` / `update_expense` / `delete_expense` | Money spent, optionally against a project. A purchase takes supplier + GST-inclusive cents (+ optional GST); a `VEHICLE_KM` trip takes distance + purpose and is priced at the ATO cents-per-km rate for its date |
| `get_vehicle_km_summary` | The caller's own trip km for a financial year against the ATO's 5,000 km cap |

Not exposed: attachments (an MCP client can't upload, and download URLs are presigned links), the
raw HTML body, and the owner-level invoicing settings and next-invoice-number (use the web app).

Connecting a client (e.g. Claude Code): `claude mcp add --transport http toolbox https://<web_domain>/mcp`,
then approve the request on the consent page. Locally, run `make dev-local` and use
`http://localhost:8000/mcp`. Integration tests: `cargo test --test mcp_dynamodb_local`.

### Connected AI apps (list + revoke)

Once a member has approved a client, `me { oauthGrants }` lists their authorized grants: client
name, redirect host, scope, created/last-used timestamps, and `refreshExpiresAt` (when the grant
goes dead if never used again). Token hashes and the client id are never exposed. Expired grants
are filtered out at read time, since DynamoDB's TTL deletion lags real expiry.

`revokeOauthGrant(id)` deletes a grant outright, so its access token stops verifying immediately.
Both are **self-only, superusers included** (the same posture as `User.passkeys`): anything else
fails the same way a missing grant would, so a caller can't probe for other users' grant ids. There
is no "revoke on disable": disabling a user already blocks every credential kind via
`fetch_update_user_auth_info`, and re-enabling restores access the way it does for other tokens.

The web page is the "Connected AI apps" section of `/app/settings`.
