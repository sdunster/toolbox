# Development setup

## 1. Install the toolchain

### Rust

Install via [rustup](https://rustup.rs). The exact compiler version is pinned in
`api/rust-toolchain.toml` — `rustup` picks it up automatically when you run anything under
`api/`. `make check-toolchain` verifies your active `rustc` matches the pin.

### Node

Node.js >= 22 (`web/.npmrc` sets `engine-strict=true`, so an older Node fails installs loudly
rather than producing confusing errors later).

### Dependencies

```bash
cd web && npm i
```

### Optional

- `actionlint` (`brew install actionlint`) — lints `.github/workflows/*.yml`, run by
  `make gha-lint` (part of `make check`).
- Terraform — required for `make check`'s infra formatting/validation step.

## 2. Environment

Copy `web/.env.local.example` to `web/.env.local`. The repo-root `.env` (committed, non-secret)
sets sane local defaults (`DB_PREFIX`, WebAuthn RP id/origin, `TURNSTILE_DISABLED=1`).

Toolbox has no application secret to speak of — sessions are opaque `mtu_{id}.{secret}` tokens whose sha256
is stored in DynamoDB, so there's no signing key anywhere. The one optional value, Cloudflare
Turnstile's `TURNSTILE_SECRET_KEY`, goes in `.env.secret` (copied from `.env.secret.example`, never
committed) and is only needed if you want to exercise the CAPTCHA path locally instead of relying
on `TURNSTILE_DISABLED=1`.

## 3. Run it

```bash
make dev-local   # DynamoDB Local + mocked SES/SQS — no AWS account needed
make dev         # against real AWS DynamoDB tables — needs AWS credentials
```

Both build the API, wait for it to answer on `:8000`, then start the Relay compiler in watch
mode and the Vite dev server on `:5173`. See the `run_dev` macro in the `Makefile`.

`make dev` needs AWS credentials for the real DynamoDB tables `infra/` creates — export the usual
`AWS_PROFILE`/SSO environment before running it (`aws sso login --profile <profile>` if you're
using an SSO profile), and see [Deploying to AWS](#9-deploying-to-aws) below for how those tables
get created in the first place.

## 4. Everyday commands

```bash
make check          # static checks only — no tests (see CLAUDE.md)
make test            # cargo test + web unit tests
make format          # cargo fmt + prettier + terraform fmt
make lint            # actionlint + clippy + eslint
```

### After changing the GraphQL API

```bash
cd api && cargo run --locked --bin export-schema > schema.graphql
cd web && npm run relay
```

`make check` diffs the committed `api/schema.graphql` against a fresh export and fails if it's
stale.

## 5. Running without AWS

```bash
make dev-local                  # DynamoDB Local + mocked SES/SQS — no AWS account needed
make local-seed                 # writes local/seed/synthetic.json into the local DB
make local-clear                # deletes app-written rows (tokens, WebAuthn state, ...), keeps the seed
```

`make local-seed` (`api/src/bin/local-seed.rs apply`) writes three seeded instances (two support,
one invoicing), an owner and an agent, a handful of inbound addresses (including a wildcard), two
fictional invoicing projects with a few unbilled billable items, and a ready-made session token for each user — all as raw DynamoDB
items from `local/seed/synthetic.json`, so their ids are exactly as committed. Nothing in the
fixture is real: `api/tests/seed_fixtures.rs` pins that every address is
`@example.com`/`@toolbox.test` and refuses anything else.

Seeded accounts (only ever valid against a `local`-prefixed database — never real):

| Role  | Email                     | Instance(s)                     | Ready-made token                        |
| ----- | ------------------------- | -------------------------------- | ---------------------------------------- |
| owner | `owner@toolbox.test`  | `acme`, `ridgeline`, `ledger`    | `mtu_SeedTokOwner.localdev0000000000000000000owner`  |
| agent | `agent@toolbox.test`  | `acme`                           | `mtu_SeedTokAgent.localdev0000000000000000000agent`  |

`ledger` is the seeded **invoicing** instance (`kind: invoicing`) — fictional business/GST/payment
details and two fictional projects with some billable items, so the invoicing pages have something to look at without a
manual `instance create --kind invoicing` first. `acme`/`ridgeline` are plain support instances
(`kind` absent, per the omit-optional-attributes house rule).

Use a token directly (`Authorization: Bearer mtu_SeedTokOwner.localdev...`) to skip the email-code flow entirely
when poking at the API by hand (`curl`, GraphiQL at `http://localhost:8000/`), or log in normally
with the seeded emails — `poem-local`'s mocked mailer prints the 6-digit code to the API's own log.

`make local-clear` deletes everything the *running app* writes on top of the seed (session tokens
minted by a real login, WebAuthn credentials, submit codes/tokens) without touching the seeded rows
themselves, so a re-`local-seed` afterward is a clean overwrite rather than a pile-up. `make
local-reset` is the blunter tool: it destroys and rebuilds every table, discarding the seed too.

`make local-mail FILE=local/mail/new-ticket.eml` feeds a raw `.eml` fixture straight into the same
parse/route/store pipeline the deployed inbound-mail Lambda uses, with no AWS involved — the
fixtures under `local/mail/` cover a plain new ticket, a reply via `+tag`, a reply via
`In-Reply-To`, a reply from an unknown sender, an autoresponder (must be dropped), a message
addressed to a wildcard address, and a cross-tenant reply attempt. Add `FRESH=1` to replay a
fixture as a brand-new delivery — without it, the idempotency key is the file's own sha256, so a
second run of the same fixture is correctly a no-op. Outbound mail sent as a result (a reply
notification, for instance) lands as a `.eml` under `MOCK_MAIL_DIR` if you've set it (see
`local/local.env`), or is just logged if you haven't.

## 6. The admin CLI and bootstrapping an organisation

`api/src/bin/cli.rs` (`cargo run --bin cli --`) is the operator tool for inspecting and writing the
DB directly — instances, inbound addresses, users, and memberships. It writes immediately; pass the
global `--dry-run` flag to see what a command *would* do without writing anything. It takes
`--db-prefix` (or the `DB_PREFIX` env var) and, unlike `local-tables`/`local-seed`, is **not**
restricted to a local database — this is the same tool that bootstraps the first real organisation
in prod.

```bash
cd api && cargo run --bin cli -- --help
```

Bootstrap sequence for a brand-new deployment's first organisation and owner:

```bash
# 1. Create the first user (the person who will own the organisation).
#    Email is trimmed and lowercased before being stored, so login is
#    effectively case-insensitive — see SCHEMA.md's `user` table entry.
cargo run --bin cli -- user create owner@yourdomain.com "Your Name"

# 2. Create the instance (the tenant organisation). `--kind` defaults to
#    `support`; pass `--kind invoicing` instead for a second, separate
#    invoicing instance (no inbound mail, no public submission — see
#    CLAUDE.md's "Invoicing" house rule). A kind is set once at creation and
#    is immutable after that.
cargo run --bin cli -- instance create "Your Company Support" your-company \
    --from-name "Your Company Support" \
    --signature "Thanks, Your Company Support" \
    --public-submission-enabled
# cargo run --bin cli -- instance create "Your Company Billing" your-company-billing --kind invoicing

# 3. Grant that user the owner role in the instance (also possible over GraphQL via
#    `addMember`, but that's superuser-only, and bootstrapping the very first
#    superuser needs this CLI step regardless — see step 5 below).
cargo run --bin cli -- member add --instance <instance_id> --user owner@yourdomain.com --role owner

# 4. Map the instance's real inbound address(es). Support instances only —
#    an invoicing instance has no inbound mail concept and this is rejected.
cargo run --bin cli -- address add --instance <instance_id> support@yourdomain.com

# 5. (Optional) Make that user a superuser, so they can manage instances/users/
#    memberships from the web admin UI too, not just tickets. Superuser is
#    admin + instance settings only — it grants no ticket access on its own
#    (see CLAUDE.md's superuser boundary house rule) — and can only ever be
#    granted here, never over GraphQL.
cargo run --bin cli -- user set-superuser owner@yourdomain.com true
```

Against the local stack, export `local/local.env` first (`set -a && . ../local/local.env && set
+a`, from `api/`) so the CLI points at DynamoDB Local instead of a real account — or just run `make
local-seed`, which does the local-stack equivalent of this whole sequence for you, twice over, from
`local/seed/synthetic.json`.

## 7. Bypassing auth for local UI work

`--dev-auth-user <id-or-email>` on `poem`/`poem-local` bypasses token verification entirely and
treats every request as that user, with their *real* permissions (memberships included) — never a
synthetic elevated principal. Only a `User` principal can be impersonated (Toolbox has no
kiosk/session-equivalent). Never enable this in a deployed environment; the Lambda binary has no
CLI to read the flag from in the first place, so it is unreachable there by construction.

## 8. Troubleshooting

_(fill in as real issues come up; don't invent hypothetical ones.)_

## 9. Deploying to AWS

`infra/` is a flat Terraform root module (`ap-southeast-2`, plus a `us-east-1` alias used only for
CloudFront's certificate). Nothing in it is hardcoded — no account id, domain, zone, or profile —
so a fork can `terraform apply` into its own account. Terraform is applied by hand; CI never gets
Terraform credentials (see `deploy-prod.yml`'s OIDC role, which is scoped to Lambda code updates,
the web bucket, and CloudFront invalidation only).

### 9.1 Prerequisites

- An AWS account, and a Route53 hosted zone for the domain you'll point `support_domain` at
  already existing in it (Terraform looks it up with `data "aws_route53_zone"`; it does not create
  one). If the zone is new, delegate it from its registrar before applying — DNS validation for the
  ACM certificate and the SES DKIM records both need it resolvable.
- An AWS CLI profile (SSO or otherwise) with enough privilege to create the resources in
  `infra/*.tf` (IAM roles/policies, Lambda, DynamoDB, S3, CloudFront, ACM, Route53, SES, SQS, SNS,
  AWS Backup).
- Terraform >= 1.9.
- A GitHub repository to push `prod` to (`your-org/toolbox` — becomes `var.github_repo`, which
  scopes the OIDC deploy role's trust policy to `repo:<github_repo>:ref:refs/heads/prod`).

### 9.2 Create the state backend

`infra/backend.tf` declares an S3 backend with **partial configuration** — no bucket name is
committed. Create (or reuse) an S3 bucket for Terraform state by hand, then:

```bash
cd infra
terraform init \
  -backend-config="bucket=<your state bucket>" \
  -backend-config="key=toolbox/terraform.tfstate" \
  -backend-config="region=<your region>" \
  -backend-config="profile=<your profile>"
```

### 9.3 Fill in tfvars

```bash
cp infra/terraform.tfvars.example infra/terraform.tfvars   # gitignored — never commit this
```

| Variable | What it is |
| --- | --- |
| `aws_account_id` | The target account's 12-digit id (constructs ARNs; has no default on purpose) |
| `aws_profile` | The AWS CLI/SSO profile Terraform runs as (no default — there's no account to assume one for until you set this up) |
| `aws_region` | Defaults to `ap-southeast-2`; only change this if you've confirmed SES email receiving is supported in the region you pick |
| `parent_zone_name` | The already-existing Route53 zone's name (e.g. `example.com`) |
| `support_domain` | Serves **both** the web app and inbound mail (e.g. `support.example.com`) — an A/AAAA alias and an MX record coexist on this one name |
| `github_repo` | `"owner/name"` — scopes the GitHub OIDC deploy role |
| `db_prefix` | Table name prefix; default `"prod"` is normally fine |
| `allowed_origins` | CORS origins for the API Lambda Function URL — normally just `["https://<support_domain>"]` |
| `alert_email` | Subscribed to the operational alert SNS topic (`monitoring.tf`) — verify you can receive at this address before applying, or the SNS subscription sits unconfirmed |
| `inbound_retention_days` | How long raw inbound MIME is kept in S3 before lifecycle expiry |
| `turnstile_secret_key` | Optional — leave blank to skip Cloudflare Turnstile verification entirely |
| `oauth_client_id_secret` | Optional — signs the client ids MCP clients register with (`openssl rand -base64 48`); leave blank and dynamic client registration answers `503`, so no MCP client can connect |

### 9.4 Import anything that already exists

Terraform assumes it is creating everything. If someone has already set part of this up by hand —
an SES domain identity verified through the console, say — `apply` fails with `AlreadyExists`
rather than adopting it. Check first, and import what is already there:

```bash
aws sesv2 list-email-identities --profile <profile> --query 'EmailIdentities[].IdentityName'

# If the domain identity already exists:
terraform import aws_sesv2_email_identity.main <support domain>
terraform import aws_sesv2_email_identity_mail_from_attributes.main <support domain>
```

After importing, `terraform plan` will show what it wants to *change* about them rather than
create — typically attaching its own configuration set in place of whatever the console made.
Read that diff before applying it.

### 9.5 Apply

```bash
aws sso login --profile <profile>   # or however your profile authenticates
cd infra
terraform plan
terraform apply
```

This creates the DynamoDB tables, the mail/web S3 buckets, the two Lambda functions (pointed at
`infra/placeholder.zip` — Terraform owns their configuration, not their code; see the
`ignore_changes` lifecycle block on each), the SES domain identity and receipt rule, the
CloudFront distribution, monitoring alarms, and the GitHub OIDC deploy role.

**The first apply blocks part-way through, on purpose.** DNS is not managed by Terraform (see
`infra/dns.tf`: the parent zone lives in a different AWS account), so `aws_acm_certificate_validation`
sits waiting for a certificate that cannot issue until you create its validation record by hand.
The sequence is:

1. `terraform apply` — it mints the certificate, prints the `dns_records_required` output, then
   waits. Let it wait, or interrupt it; nothing is lost either way.
2. Create the records that output lists in the parent zone. The **ACM validation** record is the
   one blocking you; the MX record is the one that makes inbound mail work at all.
3. `terraform apply` again. The certificate validates, CloudFront comes up, and the rest follows.

To see the records without waiting: `terraform apply -target=aws_acm_certificate.web` then
`terraform output dns_records_required`.

**Before applying against an account that might already receive mail elsewhere**, check
`aws ses describe-active-receipt-rule-set --profile <profile>` — an account has exactly one active
receipt rule set per region, and `aws_ses_active_receipt_rule_set` in `infra/ses.tf` will make
Toolbox's the one that's active.

### 9.6 Wire up GitHub

`terraform output` after a successful apply prints the values below — each output's own
`description` (in `infra/outputs.tf`) names exactly which repo variable to paste it into. Set them
under the repository's Settings → Secrets and variables → Actions → **Variables** tab (not
Secrets — none of these are sensitive on their own):

| Repo variable | From output |
| --- | --- |
| `AWS_DEPLOY_ROLE_ARN` | `github_deploy_role_arn` |
| `VITE_API_URL` | `api_url` (`https://<web_domain>/graphql`, served same-origin by CloudFront; `api_function_url` also still works) |
| `WEB_BUCKET_NAME` | `web_bucket_name` |
| `CLOUDFRONT_DISTRIBUTION_ID` | `cloudfront_distribution_id` |

Pushing to the `prod` branch (after these are set) runs `deploy-prod.yml`: all four `_check-*.yml`
gates, then `deploy_lambdas` (builds both binaries with `cargo lambda build`, assumes the OIDC role
only *after* the build finishes, then `cargo lambda deploy`s each — no AWS access during the build
itself, and no `iam:PassRole` since the functions already have their Terraform-managed execution
roles), then `deploy_web` (builds the frontend with `VITE_CLIENT_VERSION` pinned to the commit SHA,
syncs to S3 in three passes so a client mid-navigation never 404s, then invalidates
`/index.html`).

### 9.7 SES production access — do this before real use

A fresh SES identity starts in the **sandbox**: inbound receiving is unaffected, but *outbound* is
capped at 200 messages/24h, 1/sec, and can only be sent to addresses you've separately verified. In
practice this means tickets will arrive fine, but reply notifications will silently fail to reach
anyone except your own verified test address.

Request production access from the SES console (or `aws sesv2 put-account-details`) — it's a
one-off support-case-style request, usually approved within a day. Verify your own address in the
meantime (SES console → Identities → Create identity) so you can test the outbound round-trip while
waiting. Confirm you're clear with:

```bash
aws sesv2 get-account --profile <profile>   # ProductionAccessEnabled: true
```

### 9.8 Verifying the real thing

```bash
dig MX support.<your domain>                                      # 10 inbound-smtp.ap-southeast-2.amazonaws.com
aws ses describe-active-receipt-rule-set --profile <profile>      # Toolbox's rule set is active
aws sesv2 get-account --profile <profile>                         # ProductionAccessEnabled: true
```

Send a real email to one of an instance's inbound addresses — the ticket should appear in the
queue within seconds (`aws logs tail /aws/lambda/toolbox-inbound-mail --follow --profile
<profile>` if it doesn't). Reply from the web UI and confirm the reply arrives threaded in the
original mail client, and that replying to *that* lands back on the same ticket. Check the DLQ
alarm (`monitoring.tf`) is `OK`, and that a deliberately malformed message lands in the DLQ rather
than looping.

## Where to go next

- [SCHEMA.md](SCHEMA.md) — the data model.
- [README.md](README.md) — product overview, project layout, branches.
- [CONTRIBUTING.md](CONTRIBUTING.md) — PR process and checks.
