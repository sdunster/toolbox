# Toolbox

Toolbox is a small multi-tenant helpdesk / shared-inbox app. An organisation ("instance")
gets one or more inbound email addresses; mail to those addresses opens or updates a ticket.
Requesters and CCs stay in the loop by email while agents work the queue from a web admin UI.
Anyone can also raise a ticket from the public web form after verifying their email with a code.

A second, separate function — **invoicing** — lives in its own instances alongside support ones,
sharing the same instance switcher and membership roles: track clients/projects, record billable
items, turn them into invoices, and track expenses (including cents-per-km vehicle trips). Once finalized, an invoice is read-only and downloadable as a
PDF, rendered server-side from its frozen content (see CLAUDE.md's "Invoicing" house rule).

**Features:**
- Inbound email → ticket, with reply threading via `+tag` addressing and `In-Reply-To`/`References`
- Outbound replies mailed to requesters and CCs, with internal notes that are never emailed
- Multiple inbound addresses per instance, including domain wildcards (`*@sub.example.com`)
- Passwordless auth — email code or passkeys — for agents; email-code verification for public
  ticket submission
- Invoicing instances: clients/projects, billable items, invoices, and expenses — a second function
  alongside support, isolated from it but sharing the same login and instance switcher
- Finalized invoices download as a PDF, rendered server-side and cached in S3 behind a presigned,
  filename-forcing download link
- Runs on AWS Lambda + DynamoDB — scales to zero when idle

**Stack:** Rust (GraphQL API, async-graphql) · React + Relay (frontend) · AWS (Lambda, DynamoDB,
S3, SES, SQS, CloudFront) · Terraform

---

## Getting started

**→ See [DEVELOPMENT.md](DEVELOPMENT.md) for the full setup guide.** It covers installing the
toolchain, local development without an AWS account, and running the stack.

The short version:

```bash
cp web/.env.local.example web/.env.local
cd web && npm i
make dev-local   # DynamoDB Local + mocked SES/SQS, API :8000, Relay watch, web :5173
```

`make dev` runs against real AWS DynamoDB tables instead; see DEVELOPMENT.md for credentials.

Prerequisites: Rust (via [rustup](https://rustup.rs)), Node.js >= 22, and — for `make dev`
only — AWS credentials.

> **Known gaps.** Bounce and complaint handling for outbound mail is not implemented — SES
> notifications are not consumed, so a bad address is not flagged or suppressed
> ([#6](../../issues/6)). Ticket message threads are unpaged ([#1](../../issues/1)), there is no
> ticket search ([#8](../../issues/8)), and adding a member needs a superuser (CLI or web admin
> UI) rather than a self-serve email invite ([#7](../../issues/7)). The full list is in the
> [issue tracker](../../issues); `SCHEMA.md` carries the data-model-level register.

---

## Project structure

```
api/     Rust crate `toolbox` — GraphQL API lambda, inbound-mail lambda, dev server, CLI
web/     React 19 + Relay + Vite + Tailwind v4
infra/   Terraform (prod only) — nothing hardcoded, so a fork can `terraform apply`
local/   DynamoDB Local + seed + local mail fixtures
.github/ workflows
```

See [DEVELOPMENT.md](DEVELOPMENT.md) for local setup and [SCHEMA.md](SCHEMA.md) for the data
model.

---

## Branches & deployments

| Branch | Environment |
| --- | --- |
| `main` | stable branch; never force-pushed |
| `prod` | deploys to production on push |

Fork PR branches from `main`.

---

## Contributing

Contributions are welcome — bug fixes, improvements, or new features. See
[CONTRIBUTING.md](CONTRIBUTING.md) for how to submit a PR and run the checks.

---

## License

[MIT](LICENSE)
