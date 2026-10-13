//! The impure inbound-mail orchestration: idempotency, parse, loop guards,
//! resolve, store, notify. Shared by `bin/inbound-mail/` (the SQS-triggered
//! Lambda) and `bin/cli.rs`'s `mail process` subcommand (`make local-mail`)
//! — both call [`process_raw_message`] with the same raw MIME bytes; the
//! only difference is where those bytes came from (S3 vs. a local `.eml`
//! file) and which `storage::Handler`/`mail::Handler` are wired in (real S3
//! + SES for the Lambda, the local-file/log mocks for the CLI).
//!
//! # Ticket resolution and the cross-tenant check
//!
//! `inbound::resolution::build_resolution_plan` decides, purely, which
//! candidates to try and in what order. This module walks that plan against
//! the database: for each `reply_tag` candidate, a strongly-consistent
//! `GetItem` on the ticket id, then [`crate::inbound::resolution::constant_time_eq`]
//! against the stored `reply_token`; for each `message_id` candidate,
//! `rfc_message_id-index` then the same `GetItem`; for the subject-tag
//! fallback, `instance_number-index` keyed on the *already-resolved*
//! `instance_id` (never the subject's own slug).
//!
//! **A ticket resolved by tag or message-id must belong to the instance the
//! mail actually routed to.** A forged or stolen `+t{ticket_id}.{reply_token}`
//! tag — or an `In-Reply-To` copied from a different tenant's thread — that
//! nonetheless verifies correctly but names a ticket in a *different*
//! instance is a cross-tenant write attempt, not an ordinary non-match: it
//! means someone (or something) is trying to make content land in a tenant
//! it doesn't belong to, using a credential (the token, or the
//! provider-assigned message id) that is only ever valid for its own
//! tenant. This is treated as a hard rejection of the whole message — it
//! does **not** fall through to the next resolution step or open a new
//! ticket — rather than a soft "try the next candidate", because falling
//! through would still let an attacker who knows this reject-and-continue
//! behaviour probe for which tickets exist elsewhere. The subject-tag path
//! has no equivalent check because it can't produce a cross-tenant result in
//! the first place: its lookup key already includes the resolved
//! `instance_id`, so a hit is structurally confined to that instance.

use anyhow::{Context, Result};
use mail_parser::{Message, MimeHeaders};

use crate::app::{App, HasDb, HasMail, HasStorage};
use crate::db;
use crate::db::Handler as _;
use crate::inbound::{attachments, resolution, routing};
use crate::mail::Handler as _;
use crate::mailloop::{self, DropReason};
use crate::outbound;
use crate::staff_notify::{self, Actor, StaffEvent};
use crate::storage;
use crate::suppression;

/// How long a `processed_message` idempotency row is retained. Long enough
/// to outlast SQS's redelivery window (the queue's visibility timeout plus
/// its DLQ retention, 14 days per the build plan's `sqs.tf` sketch), not
/// indefinitely — see `SCHEMA.md`.
const PROCESSED_MESSAGE_RETENTION_SECS: u64 = 60 * 60 * 24 * 14;

/// Placeholder subject for a new ticket opened by a message with no
/// (usable) `Subject:` header.
const NO_SUBJECT_PLACEHOLDER: &str = "(no subject)";

/// What happened to one raw message. Every field is `Some`/non-default only
/// when the pipeline got that far — a dropped message has `ticket_id: None`
/// and `dropped_reason: Some(...)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub dropped_reason: Option<String>,
    pub ticket_id: Option<String>,
    pub message_id: Option<String>,
    pub created_new_ticket: bool,
    pub reopened: bool,
    pub added_sender_as_cc: bool,
    pub attachment_count: usize,
}

impl Outcome {
    fn dropped(reason: impl Into<String>) -> Self {
        Self {
            dropped_reason: Some(reason.into()),
            ..Default::default()
        }
    }
}

/// Process one raw RFC 5322 message end to end. `ses_message_id` is the
/// idempotency key (SES's own message id for the Lambda; a caller-chosen
/// stable id, derived from the file's own content, for `make local-mail`).
/// `raw_s3_key`, when given, is stamped onto the created `inbound`
/// `ticket_message` row (`SetRawS3Key`) — `None` for the AWS-free
/// `local-mail` path, which has nowhere to have stored the raw bytes.
pub async fn process_raw_message<A>(
    app: &A,
    ses_message_id: &str,
    raw: &[u8],
    raw_s3_key: Option<&str>,
) -> Result<Outcome>
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync,
{
    let now = crate::clock::now_sec();

    // 1. Idempotency — see db::Handler::claim_processed_message's doc
    // comment for the crash-window trade this makes.
    let expires_at = now + PROCESSED_MESSAGE_RETENTION_SECS;
    let claimed = app
        .db()
        .claim_processed_message(ses_message_id, now, expires_at)
        .await?;
    if !claimed {
        tracing::info!(ses_message_id, "duplicate SES message id; skipping");
        return Ok(Outcome::dropped("duplicate SES message id"));
    }

    // 2. Fetch + parse. Fetching is the caller's job (S3 for the Lambda,
    // already-in-hand bytes for local-mail) — this function only parses.
    let Some(msg) = mail_parser::MessageParser::new().parse(raw) else {
        tracing::warn!(ses_message_id, "raw message failed to parse; dropping");
        return Ok(Outcome::dropped("failed to parse as RFC 5322"));
    };

    let from = first_address(msg.from());
    let to = all_addresses(msg.to());
    let cc = all_addresses(msg.cc());
    let recipients: Vec<String> = to.iter().chain(cc.iter()).cloned().collect();
    let subject = msg.subject().unwrap_or_default().to_string();
    let arriving_message_id = msg.message_id().map(|s| format!("<{s}>"));
    let in_reply_to = header_value_string(msg.in_reply_to());
    let references = header_value_string(msg.references());

    // 3. Loop prevention (structural).
    let loop_headers = mailloop::LoopCheckHeaders {
        auto_submitted: msg.header_raw("Auto-Submitted").map(str::trim),
        precedence: msg.header_raw("Precedence").map(str::trim),
        loop_header_present: msg.header("X-Toolbox-Loop").is_some(),
    };
    if let Some(reason) = mailloop::check_structural(&loop_headers) {
        tracing::info!(ses_message_id, %reason, "dropping (loop guard)");
        return Ok(Outcome::dropped(reason.to_string()));
    }

    // 4. Resolve the instance.
    let Some(instance_id) = routing::resolve_instance_id(app.db(), &recipients).await? else {
        tracing::info!(
            ses_message_id,
            ?recipients,
            "no matching inbound address; dropping"
        );
        return Ok(Outcome::dropped(
            "no recipient matched a known inbound address",
        ));
    };

    // A recipient can resolve to an instance that has since been
    // soft-deleted — the `inbound_address` row itself isn't touched by
    // `setInstanceDeleted`, only `instance.deleted`. Mirror the "no matching
    // inbound address" path exactly: log it, drop the message, create no
    // ticket, send no mail. The alternative (keep opening tickets against a
    // deleted instance) is silently wrong in a way nobody would notice until
    // someone finally looks at that instance's ticket queue again.
    let instance = app
        .db()
        .get_instances(&[instance_id.as_str()])
        .await?
        .into_iter()
        .next()
        .flatten();
    // Kind isolation (CLAUDE.md): mail routed to an invoicing instance is
    // dropped exactly like mail to a deleted one — invoicing has no inbound
    // mail concept at all, so there is nothing legitimate for this message
    // to become.
    if instance.is_none_or(|i| i.deleted || i.kind != db::InstanceKind::Support) {
        tracing::info!(
            ses_message_id,
            instance_id,
            "resolved instance is deleted or not a support instance; dropping"
        );
        return Ok(Outcome::dropped(
            "resolved instance is deleted or not a support instance",
        ));
    }

    // Loop prevention, part 4: From is one of our own inbound addresses —
    // this needs a DB lookup (see mailloop's module doc), so it happens
    // here rather than in the structural check above.
    if let Some(from_addr) = &from
        && routing::resolve_instance_id(app.db(), std::slice::from_ref(from_addr))
            .await?
            .is_some()
    {
        let reason = DropReason::FromOwnAddress(from_addr.clone());
        tracing::info!(ses_message_id, %reason, "dropping (loop guard)");
        return Ok(Outcome::dropped(reason.to_string()));
    }

    let Some(from_raw) = from else {
        tracing::warn!(ses_message_id, "message has no From address; dropping");
        return Ok(Outcome::dropped("no From address"));
    };
    let from_normalized = routing::normalize_recipient(&from_raw).address;

    // 5. Resolve the ticket.
    let plan = resolution::build_resolution_plan(
        &recipients,
        in_reply_to.as_deref(),
        references.as_deref(),
        &subject,
    );
    let resolved = match resolve_ticket(app, &plan, &instance_id).await? {
        TicketLookup::CrossTenant => {
            tracing::warn!(
                ses_message_id,
                instance_id,
                "resolved a ticket belonging to a different instance; rejecting message"
            );
            return Ok(Outcome::dropped(
                "resolved ticket belongs to a different instance (cross-tenant reject)",
            ));
        }
        TicketLookup::Found(t) => Some(t),
        TicketLookup::NotFound => None,
    };

    let addresses = app
        .db()
        .list_inbound_addresses_by_instance(&instance_id)
        .await?;

    let (ticket, created_new_ticket, mut added_sender_as_cc, reopened) = match resolved {
        Some(ticket) => {
            // 7. Existing ticket: append, bump activity, reopen if closed.
            let added_as_cc = !ticket.requester_emails.contains(&from_normalized)
                && !ticket.cc_emails.contains(&from_normalized);
            if added_as_cc {
                // Safe to add unconditionally: the sender already possesses
                // either the ticket's unguessable reply token (tag match)
                // or the id of a message genuinely in this thread
                // (In-Reply-To/References match) — either way, they are
                // someone this thread was already shared with, not an
                // arbitrary third party.
                app.db()
                    .update_ticket(
                        &ticket.id,
                        db::TicketUpdateShape::AddCc {
                            email: &from_normalized,
                            now,
                        },
                    )
                    .await?;
            }
            let was_closed = ticket.status == db::TicketStatus::Closed;
            if was_closed {
                app.db()
                    .update_ticket(
                        &ticket.id,
                        db::TicketUpdateShape::SetStatusAndAssignee {
                            instance_id: &instance_id,
                            status: db::TicketStatus::Open,
                            assignee_user_id: ticket.assignee_user_id.as_deref(),
                            now,
                        },
                    )
                    .await?;
            } else {
                app.db()
                    .update_ticket(&ticket.id, db::TicketUpdateShape::Touch { now })
                    .await?;
            }
            (*ticket, false, added_as_cc, was_closed)
        }
        None => {
            // 6. New ticket.
            let subject_for_ticket = if subject.trim().is_empty() {
                NO_SUBJECT_PLACEHOLDER.to_string()
            } else {
                subject.clone()
            };
            let ccs = strip_own_addresses(&addresses, &to, &cc, &from_normalized);
            let number = app.db().increment_ticket_counter(&instance_id).await?;
            let ticket = app
                .db()
                .create_ticket(
                    &instance_id,
                    number,
                    &subject_for_ticket,
                    std::slice::from_ref(&from_normalized),
                    &ccs,
                )
                .await?;
            (ticket, true, false, false)
        }
    };
    // Re-fetch when we didn't just create it and didn't touch CCs, so the
    // in-memory `ticket.cc_emails`/`status` used below (for notify
    // recipients) reflects what was actually written. Cheap relative to the
    // writes already performed, and keeps this function's control flow
    // linear instead of hand-threading every field mutation through.
    let ticket = if created_new_ticket {
        ticket
    } else {
        app.db()
            .get_tickets(&[ticket.id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .unwrap_or(ticket)
    };
    if added_sender_as_cc && !ticket.cc_emails.contains(&from_normalized) {
        // The re-fetch should already reflect the write above; this is
        // belt-and-braces in case of an eventual-consistency surprise on a
        // backend other than the strongly-consistent GetItem this project
        // always uses (there is none today, but nothing above allows a
        // stale read to silently change behaviour).
        added_sender_as_cc = true;
    }

    let body_text = msg.body_text(0).map(|c| c.to_string());
    let body_html = msg.body_html(0).map(|c| c.to_string());

    let message = app
        .db()
        .create_ticket_message(
            &ticket.id,
            db::TicketMessageKind::Inbound,
            None,
            Some(&from_normalized),
            &to,
            &cc,
            body_text.as_deref(),
            body_html.as_deref(),
            in_reply_to.as_deref(),
            references.as_deref(),
        )
        .await?;

    if let Some(mid) = &arriving_message_id {
        app.db()
            .update_ticket_message(
                &message.id,
                db::TicketMessageUpdateShape::SetRfcMessageId {
                    rfc_message_id: mid,
                },
            )
            .await?;
    }
    if let Some(key) = raw_s3_key {
        app.db()
            .update_ticket_message(
                &message.id,
                db::TicketMessageUpdateShape::SetRawS3Key { raw_s3_key: key },
            )
            .await?;
    }

    // 8. Attachments.
    let stored_attachments =
        store_attachments(app.storage(), &msg, &ticket.id, &message.id).await?;
    if !stored_attachments.is_empty() {
        app.db()
            .update_ticket_message(
                &message.id,
                db::TicketMessageUpdateShape::SetAttachments {
                    attachments: &stored_attachments,
                },
            )
            .await?;
        // Denormalise onto the ticket — see Ticket::has_attachments.
        app.db()
            .update_ticket(&ticket.id, db::TicketUpdateShape::MarkHasAttachments)
            .await?;
    }

    // 9. Notify — forward this activity to every requester/CC who wasn't
    // the sender, so participants added purely through the ticket system
    // (not present on the arriving mail's own To/Cc) still hear about it.
    // See this module's doc comment header for why this differs from an
    // "acknowledgement": the sender is always excluded, so a brand-new
    // single-requester ticket with no CCs sends nothing at all.
    let notify_body = body_text
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "(no message body)".to_string());
    let notify_recipients: Vec<String> = ticket
        .requester_emails
        .iter()
        .chain(ticket.cc_emails.iter())
        .filter(|e| **e != from_normalized)
        .cloned()
        .collect();
    let outbound_message_id = if notify_recipients.is_empty() {
        None
    } else {
        notify_others(
            app,
            &ticket,
            &notify_recipients,
            &notify_body,
            ses_message_id,
        )
        .await
    };

    // 10. Best-effort staff notice — see `staff_notify`'s module doc. Uses
    // the re-fetched `ticket` (post-update state), and the raw `body_text`
    // (not `notify_body`'s customer-relay fallback text) — `staff_notify`
    // applies its own "(no message body)" placeholder for an empty/missing
    // text part.
    let staff_event = if created_new_ticket {
        StaffEvent::NewTicket {
            requester_email: &from_normalized,
        }
    } else {
        StaffEvent::CustomerMessage {
            from_email: &from_normalized,
            reopened,
        }
    };
    staff_notify::notify_staff(
        app,
        &ticket,
        ticket.assignee_user_id.as_deref(),
        staff_event,
        Actor::Email(&from_normalized),
        Some(body_text.as_deref().unwrap_or("")),
    )
    .await;

    Ok(Outcome {
        dropped_reason: None,
        ticket_id: Some(ticket.id.clone()),
        message_id: Some(message.id.clone()),
        created_new_ticket,
        reopened,
        added_sender_as_cc,
        attachment_count: stored_attachments.len(),
    })
    .inspect(|_| {
        if let Some(id) = outbound_message_id {
            tracing::debug!(ses_message_id, notify_message_id = %id, "notified other participants");
        }
    })
}

/// The outcome of walking a [`resolution::ResolutionPlan`] against the
/// database.
enum TicketLookup {
    Found(Box<db::Ticket>),
    NotFound,
    /// A candidate verified correctly but named a ticket in a different
    /// instance — see this module's doc comment.
    CrossTenant,
}

async fn resolve_ticket<A: App + HasDb + Send + Sync>(
    app: &A,
    plan: &resolution::ResolutionPlan,
    instance_id: &str,
) -> Result<TicketLookup> {
    for tag in &plan.reply_tags {
        let Some(ticket) = app
            .db()
            .get_tickets(&[tag.ticket_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
        else {
            continue;
        };
        if !resolution::constant_time_eq(ticket.reply_token.as_bytes(), tag.reply_token.as_bytes())
        {
            continue;
        }
        if ticket.instance_id != instance_id {
            return Ok(TicketLookup::CrossTenant);
        }
        return Ok(TicketLookup::Found(Box::new(ticket)));
    }

    for mid in &plan.message_ids {
        let Some(ticket_id) = app.db().get_ticket_id_by_rfc_message_id(mid).await? else {
            continue;
        };
        let Some(ticket) = app
            .db()
            .get_tickets(&[ticket_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
        else {
            continue;
        };
        if ticket.instance_id != instance_id {
            return Ok(TicketLookup::CrossTenant);
        }
        return Ok(TicketLookup::Found(Box::new(ticket)));
    }

    if let Some(number) = plan.subject_number {
        // Keyed on the already-resolved instance_id, not the subject's own
        // slug — see this module's doc comment for why that makes a hit
        // here structurally incapable of naming another tenant's ticket.
        if let Some(ticket_id) = app
            .db()
            .get_ticket_id_by_instance_number(instance_id, number)
            .await?
            && let Some(ticket) = app
                .db()
                .get_tickets(&[ticket_id.as_str()])
                .await?
                .into_iter()
                .next()
                .flatten()
        {
            return Ok(TicketLookup::Found(Box::new(ticket)));
        }
    }

    Ok(TicketLookup::NotFound)
}

/// Store every attachment on `msg` under
/// `attachments/{ticket_id}/{message_id}/{n}/{filename}`, skipping (and
/// logging) any that exceed [`attachments::MAX_ATTACHMENT_BYTES`] — one
/// oversized attachment must not fail the whole message.
async fn store_attachments(
    storage: &impl storage::Handler,
    msg: &Message<'_>,
    ticket_id: &str,
    message_id: &str,
) -> Result<Vec<db::Attachment>> {
    let mut stored = Vec::new();
    for (index, part) in msg.attachments().enumerate() {
        let bytes = part.contents();
        if bytes.len() > attachments::MAX_ATTACHMENT_BYTES {
            tracing::warn!(
                ticket_id,
                message_id,
                index,
                size = bytes.len(),
                "attachment exceeds the size cap; skipping"
            );
            continue;
        }
        let filename = attachments::sanitize_filename(part.attachment_name(), index);
        let content_type = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{sub}", ct.ctype()),
                None => ct.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let key = attachments::attachment_key(ticket_id, message_id, index, &filename);
        storage
            .put_bytes(&key, bytes, &content_type)
            .await
            .with_context(|| format!("storing attachment {key}"))?;
        stored.push(db::Attachment {
            s3_key: key,
            filename,
            content_type,
            size: bytes.len() as u64,
        });
    }
    Ok(stored)
}

/// Best-effort relay of `body` to `recipients` (already filtered to exclude
/// the sender), stored as a `System` `ticket_message` row so a reply to it
/// threads back via `rfc_message_id-index` like any other outbound message.
/// Mirrors `graphql::mutations::send_system_notification`'s
/// log-and-swallow-on-failure shape: the inbound message itself is already
/// safely stored by the time this runs, so a delivery failure here must not
/// turn into a dropped/duplicated inbound message on SQS retry.
async fn notify_others<A: App + HasDb + HasMail + Send + Sync>(
    app: &A,
    ticket: &db::Ticket,
    recipients: &[String],
    body: &str,
    ses_message_id: &str,
) -> Option<String> {
    let instance = match app.db().get_instances(&[ticket.instance_id.as_str()]).await {
        Ok(v) => v.into_iter().next().flatten(),
        Err(e) => {
            tracing::warn!(ses_message_id, "notify: could not load instance: {e:#}");
            return None;
        }
    };
    let Some(instance) = instance else {
        tracing::warn!(ses_message_id, "notify: instance missing");
        return None;
    };
    let addresses = match app
        .db()
        .list_inbound_addresses_by_instance(&instance.id)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                ses_message_id,
                "notify: could not load inbound addresses: {e:#}"
            );
            return None;
        }
    };
    let prior = match app.db().list_ticket_messages(&ticket.id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                ses_message_id,
                "notify: could not load prior messages: {e:#}"
            );
            return None;
        }
    };
    let threading = outbound::threading_for(&prior);

    // Suppressed addresses (`crate::suppression`) are skipped here like
    // every other instance mail.
    let recipients = suppression::filter_recipients(app, recipients, &[], "notify")
        .await
        .to;
    if recipients.is_empty() {
        tracing::info!(
            ses_message_id,
            "notify: every other recipient is suppressed; nothing sent"
        );
        return None;
    }

    let built = match outbound::build_outbound(
        &instance,
        &addresses,
        ticket,
        &recipients,
        &[],
        body,
        &threading,
    ) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                ses_message_id,
                "notify: could not build outbound message: {e:#}"
            );
            return None;
        }
    };
    let rfc_message_id = match app
        .mail()
        .send_raw_tagged(
            &built.raw,
            &built.to,
            &built.cc,
            &crate::ticket_delivery::send_tags(&ticket.id),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(ses_message_id, "notify: send failed: {e:#}");
            return None;
        }
    };
    let msg = match app
        .db()
        .create_ticket_message(
            &ticket.id,
            db::TicketMessageKind::System,
            None,
            None,
            &built.to,
            &built.cc,
            Some(body),
            None,
            threading.in_reply_to.as_deref(),
            threading.references.as_deref(),
        )
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                ses_message_id,
                "notify: sent but could not persist row: {e:#}"
            );
            return None;
        }
    };
    if let Err(e) = app
        .db()
        .update_ticket_message(
            &msg.id,
            db::TicketMessageUpdateShape::SetRfcMessageId {
                rfc_message_id: &rfc_message_id,
            },
        )
        .await
    {
        tracing::warn!(
            ses_message_id,
            "notify: could not stamp rfc_message_id: {e:#}"
        );
    }
    Some(rfc_message_id)
}

/// This instance's own addresses (exact match, or same domain as one of its
/// wildcards) filtered out of a candidate list — the pure counterpart to
/// `outbound::is_own_address` (kept private there), duplicated here in
/// miniature rather than made `pub` across a module boundary for one small
/// filter. Also excludes the sender itself and deduplicates.
fn strip_own_addresses(
    addresses: &[db::InboundAddress],
    to: &[String],
    cc: &[String],
    from_normalized: &str,
) -> Vec<String> {
    let is_own = |candidate: &str| {
        let n = routing::normalize_recipient(candidate);
        addresses.iter().any(|a| match a.kind {
            db::AddressKind::Exact => a.address == n.address,
            db::AddressKind::Wildcard => a.address.strip_prefix("*@") == n.domain.as_deref(),
        })
    };
    let mut out: Vec<String> = Vec::new();
    for raw in to.iter().chain(cc.iter()) {
        let normalized = routing::normalize_recipient(raw).address;
        if normalized == from_normalized || is_own(raw) || out.contains(&normalized) {
            continue;
        }
        out.push(normalized);
    }
    out
}

fn first_address(addr: Option<&mail_parser::Address<'_>>) -> Option<String> {
    addr.and_then(|a| a.first())
        .and_then(|a| a.address())
        .map(str::to_string)
}

fn all_addresses(addr: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    addr.iter()
        .filter_map(|a| a.address())
        .map(str::to_string)
        .collect()
}

/// Extract `In-Reply-To`/`References` as a space-separated string of
/// `<id>`-bracketed message ids. `mail-parser`'s id parser
/// ([`mail_parser::parsers::fields::id`]) strips the RFC 5322 `<...>`
/// delimiters when it builds `HeaderValue::Text`/`TextList` — but every
/// `rfc_message_id` this project stores (SES's own rewritten form, and the
/// `<{id}>` this pipeline stamps on an arriving message — see
/// `arriving_message_id` above) keeps them. Re-wrapping here is what makes
/// `resolution::candidate_message_ids`' output line up with what
/// `db::Handler::get_ticket_id_by_rfc_message_id` actually has stored;
/// without it, every `In-Reply-To`/`References` lookup would silently miss.
fn header_value_string(hv: &mail_parser::HeaderValue<'_>) -> Option<String> {
    if let Some(t) = hv.as_text() {
        return Some(format!("<{t}>"));
    }
    if let Some(list) = hv.as_text_list()
        && !list.is_empty()
    {
        return Some(
            list.iter()
                .map(|s| format!("<{s}>"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    None
}
