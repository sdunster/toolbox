//! Integration tests for the inbound-mail pipeline
//! (`toolbox::inbound::pipeline::process_raw_message`) against
//! **DynamoDB Local** and the mock mail/storage handlers — proving the
//! whole parse → route → resolve → store → notify path end to end, the way
//! the unit tests in `src/inbound/*.rs` (pure functions only) and
//! `src/mailloop.rs` cannot: this is what actually exercises
//! `db::Handler::claim_processed_message`, `get_ticket_id_by_rfc_message_id`,
//! `get_ticket_id_by_instance_number`, and the real GSIs those two read
//! from.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test inbound_pipeline_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured. Every row is
//! created fresh per test run (nanoid'd instance/user/ticket ids), so
//! repeated runs never collide with each other or with `make
//! local-seed`'s fixture rows — this file never reads `local/seed/synthetic.json`.

use toolbox::app;
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::inbound::pipeline;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;

async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "inbound_pipeline_dynamodb_local: {endpoint} is configured but not reachable — \
             skipping. Run `make local-up && make local-tables` first."
        );
        return None;
    }

    Some(prefix)
}

/// A unique SES message id per call.
///
/// These must never be hardcoded constants: `processed_message` is the
/// pipeline's idempotency ledger, so a fixed id makes a test pass exactly
/// once per database and report "duplicate SES message id" on every run
/// after that — including in CI on any runner that reuses a volume.
fn ses_message_id() -> String {
    format!("ses-test-{}", nanoid::nanoid!(16))
}

/// A unique RFC 5322 Message-ID per call.
///
/// Like [`ses_message_id`], never a constant: `rfc_message_id` backs a GSI the
/// threading fallback queries, and `at_most_one` treats two messages sharing
/// one as a data-integrity error. A fixed id here makes the suite fail on its
/// second run against the same database.
fn rfc_message_id(label: &str) -> String {
    format!("<{label}-{}@customer.example.com>", nanoid::nanoid!(12))
}

macro_rules! require_local_db {
    () => {
        match local_db_prefix().await {
            Some(prefix) => prefix,
            None => {
                eprintln!(
                    "inbound_pipeline_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not \
                     set to a reachable local DynamoDB — skipping. See this file's header."
                );
                return;
            }
        }
    };
}

fn unique_id(label: &str) -> String {
    format!("{label}-{}", nanoid::nanoid!(8)).to_lowercase()
}

fn unique_email(label: &str) -> String {
    // Lowercased deliberately: the pipeline normalises addresses when it stores
    // them, so that Bob@… and bob@… cannot become two requesters on one ticket
    // who each get mailed. A mixed-case nanoid here would only be asserting
    // against that normalisation.
    format!("{label}-{}@example.com", nanoid::nanoid!(8)).to_lowercase()
}

fn test_app(db: dynamodb::Handler, storage_dir_label: &str) -> TestApp {
    let dir = std::env::temp_dir().join(format!(
        "toolbox-inbound-pipeline-test-{storage_dir_label}-{}",
        nanoid::nanoid!(8)
    ));
    app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::with_dir(dir),
        mockqueue::Queue::new(),
        0,
    )
}

/// An instance with one exact inbound address (`support@{domain}`) and one
/// wildcard-domain instance for the cross-tenant test — returns
/// `(instance, domain, support_address)`.
async fn setup_instance(db: &dynamodb::Handler, label: &str) -> (db::Instance, String, String) {
    let instance = db
        .create_instance(
            &format!("{label} instance"),
            &unique_id(&format!("{label}-slug")),
            &format!("{label} Support"),
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let domain = format!("{}.toolbox.test", unique_id(label));
    let address = format!("support@{domain}");
    db.create_inbound_address(&address, &instance.id, db::AddressKind::Exact)
        .await
        .expect("create_inbound_address");
    (instance, domain, address)
}

/// A minimal RFC 5322 message, headers first (in order), then a blank line,
/// then the body. `extra_headers` lets each test add exactly the headers it
/// needs (`To`, `In-Reply-To`, `Auto-Submitted`, ...) without a builder.
fn build_eml(from: &str, extra_headers: &[(&str, &str)], subject: &str, body: &str) -> Vec<u8> {
    let mut out = format!("From: {from}\r\n");
    for (name, value) in extra_headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!("Subject: {subject}\r\n"));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("\r\n");
    out.push_str(body);
    out.into_bytes()
}

/// A multipart message with exactly one attachment, base64-encoded.
fn build_eml_with_attachment(
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    filename: &str,
    attachment_bytes: &[u8],
) -> Vec<u8> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(attachment_bytes);
    format!(
        "From: {from}\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\r\n\
         \r\n\
         --BOUNDARY\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {body}\r\n\
         --BOUNDARY\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Transfer-Encoding: base64\r\n\
         Content-Disposition: attachment; filename=\"{filename}\"\r\n\
         \r\n\
         {encoded}\r\n\
         --BOUNDARY--\r\n"
    )
    .into_bytes()
}

fn reply_tag(ticket: &db::Ticket) -> String {
    format!("t{}.{}", ticket.id, ticket.reply_token)
}

#[tokio::test]
async fn new_ticket_created_with_right_requester_and_ccs() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, domain, support_address) = setup_instance(&db, "newtick").await;
    let app = test_app(db.clone(), "new-ticket");

    let requester = unique_email("requester");
    let cc = unique_email("watcher");
    let inbound_id = rfc_message_id("inbound");
    let raw = build_eml(
        &requester,
        &[
            ("To", &support_address),
            ("Cc", &cc),
            ("Message-ID", &inbound_id),
        ],
        "Help, my widget is broken",
        "It just stopped working.",
    );

    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .expect("process_raw_message");

    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(outcome.created_new_ticket);
    let ticket_id = outcome.ticket_id.expect("ticket_id");
    let ticket = db
        .get_tickets(&[ticket_id.as_str()])
        .await
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .expect("ticket exists");
    assert_eq!(ticket.instance_id, instance.id);
    assert_eq!(ticket.requester_emails, vec![requester.clone()]);
    assert_eq!(ticket.cc_emails, vec![cc.clone()]);
    assert_eq!(ticket.subject, "Help, my widget is broken");
    assert_eq!(ticket.status, db::TicketStatus::Open);

    let messages = db.list_ticket_messages(&ticket_id).await.unwrap();
    // Two rows, not one: this fixture has a CC, and the pipeline notifies every
    // participant *except* the sender, persisting that notification as a System
    // row so the thread records what went out. A single-requester ticket with no
    // CCs would leave just the inbound message.
    // Compared as a multiset, not a sequence: both rows are written in the same
    // second, and `created_at` is second-granular, so their relative order is
    // stable but not meaningful (see list_ticket_messages' comment).
    let mut kinds = messages.iter().map(|m| m.kind).collect::<Vec<_>>();
    kinds.sort_by_key(|k| format!("{k:?}"));
    assert_eq!(
        kinds,
        vec![
            db::TicketMessageKind::Inbound,
            db::TicketMessageKind::System
        ]
    );
    let inbound = messages
        .iter()
        .find(|m| m.kind == db::TicketMessageKind::Inbound)
        .expect("the inbound message");
    assert_eq!(inbound.from_email.as_deref(), Some(requester.as_str()));
    assert_eq!(inbound.rfc_message_id.as_deref(), Some(inbound_id.as_str()));
    let _ = domain; // kept for readability of setup_instance's return shape
}

#[tokio::test]
async fn tag_reply_appends_instead_of_creating() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_instance, _domain, support_address) = setup_instance(&db, "tagreply").await;
    let requester = unique_email("requester");
    let number = db.increment_ticket_counter(&_instance.id).await.unwrap();
    let ticket = db
        .create_ticket(
            &_instance.id,
            number,
            "Original subject",
            std::slice::from_ref(&requester),
            &[],
        )
        .await
        .unwrap();

    let app = test_app(db.clone(), "tag-reply");
    let local = support_address.split('@').next().unwrap();
    let domain = support_address.split('@').nth(1).unwrap();
    let tagged_to = format!("{local}+{}@{domain}", reply_tag(&ticket));

    let raw = build_eml(
        &requester,
        &[("To", &tagged_to)],
        "Re: Original subject",
        "Following up.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(!outcome.created_new_ticket);
    assert_eq!(outcome.ticket_id.as_deref(), Some(ticket.id.as_str()));

    let messages = db.list_ticket_messages(&ticket.id).await.unwrap();
    assert_eq!(messages.len(), 1, "must append, not create a second ticket");
}

#[tokio::test]
async fn unknown_sender_on_a_known_thread_is_added_to_cc() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "unkcc").await;
    let requester = unique_email("requester");
    let number = db.increment_ticket_counter(&instance.id).await.unwrap();
    let ticket = db
        .create_ticket(
            &instance.id,
            number,
            "Thread",
            std::slice::from_ref(&requester),
            &[],
        )
        .await
        .unwrap();

    let app = test_app(db.clone(), "unknown-cc");
    let local = support_address.split('@').next().unwrap();
    let domain = support_address.split('@').nth(1).unwrap();
    let tagged_to = format!("{local}+{}@{domain}", reply_tag(&ticket));

    let stranger = unique_email("stranger");
    let raw = build_eml(
        &stranger,
        &[("To", &tagged_to)],
        "Re: Thread",
        "I was forwarded this — same issue here.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();
    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(outcome.added_sender_as_cc);

    let updated = db
        .get_tickets(&[ticket.id.as_str()])
        .await
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .unwrap();
    assert!(updated.cc_emails.contains(&stranger));
    assert!(!updated.requester_emails.contains(&stranger));
}

#[tokio::test]
async fn autoresponder_creates_nothing() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "auto").await;
    let app = test_app(db.clone(), "autoresponder");

    let raw = build_eml(
        &unique_email("autoresponder"),
        &[("To", &support_address), ("Auto-Submitted", "auto-replied")],
        "Automatic reply",
        "Out of office.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(outcome.dropped_reason.is_some(), "{outcome:?}");
    assert!(outcome.ticket_id.is_none());

    let tickets = db
        .list_tickets(
            &instance.id,
            db::TicketListFilter::Visible,
            db::ListTicketsPage {
                after: None,
                before: None,
                limit: 10,
                descending: true,
            },
        )
        .await
        .unwrap();
    assert!(
        tickets.is_empty(),
        "autoresponder must create no ticket: {tickets:?}"
    );
}

#[tokio::test]
async fn closed_ticket_reopens_on_a_new_reply() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "reopen").await;
    let requester = unique_email("requester");
    let number = db.increment_ticket_counter(&instance.id).await.unwrap();
    let ticket = db
        .create_ticket(
            &instance.id,
            number,
            "Closed thread",
            std::slice::from_ref(&requester),
            &[],
        )
        .await
        .unwrap();
    db.update_ticket(
        &ticket.id,
        db::TicketUpdateShape::SetStatusAndAssignee {
            instance_id: &instance.id,
            status: db::TicketStatus::Closed,
            assignee_user_id: None,
            now: toolbox::clock::now_sec(),
        },
    )
    .await
    .unwrap();

    let app = test_app(db.clone(), "reopen");
    let local = support_address.split('@').next().unwrap();
    let domain = support_address.split('@').nth(1).unwrap();
    let tagged_to = format!("{local}+{}@{domain}", reply_tag(&ticket));
    let raw = build_eml(
        &requester,
        &[("To", &tagged_to)],
        "Re: Closed thread",
        "Actually this is still broken.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();
    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(outcome.reopened);

    let updated = db
        .get_tickets(&[ticket.id.as_str()])
        .await
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .unwrap();
    assert_eq!(updated.status, db::TicketStatus::Open);
}

#[tokio::test]
async fn a_cross_tenant_tag_is_rejected() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_a, _domain_a, _address_a) = setup_instance(&db, "tenanta").await;
    let (instance_b, domain_b, _address_b) = setup_instance(&db, "tenantb").await;
    // A wildcard on instance B's domain, so any local part routes there.
    let wildcard_b = format!("*@{domain_b}");
    db.create_inbound_address(&wildcard_b, &instance_b.id, db::AddressKind::Wildcard)
        .await
        .unwrap();

    let requester = unique_email("requester");
    let number = db.increment_ticket_counter(&instance_a.id).await.unwrap();
    let ticket_a = db
        .create_ticket(
            &instance_a.id,
            number,
            "Instance A's ticket",
            std::slice::from_ref(&requester),
            &[],
        )
        .await
        .unwrap();

    let app = test_app(db.clone(), "cross-tenant");
    // A syntactically valid tag for ticket_a's ticket+token, but addressed
    // at instance B's wildcard domain.
    let forged_to = format!("anything+{}@{domain_b}", reply_tag(&ticket_a));
    let raw = build_eml(
        &unique_email("attacker"),
        &[("To", &forged_to)],
        "Re: Instance A's ticket",
        "Trying to write into another tenant's ticket.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(
        outcome
            .dropped_reason
            .as_deref()
            .is_some_and(|r| r.contains("cross-tenant")),
        "{outcome:?}"
    );
    assert!(outcome.ticket_id.is_none());
    assert!(outcome.message_id.is_none());

    // And nothing was appended to instance A's ticket either.
    let messages = db.list_ticket_messages(&ticket_a.id).await.unwrap();
    assert!(messages.is_empty(), "{messages:?}");
}

#[tokio::test]
async fn a_duplicate_ses_message_id_is_a_no_op() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_instance, _domain, support_address) = setup_instance(&db, "dupe").await;
    let app = test_app(db.clone(), "duplicate");

    let raw = build_eml(
        &unique_email("requester"),
        &[("To", &support_address)],
        "Dupe test",
        "First delivery.",
    );

    // One id used twice on purpose — that is the whole point of this test.
    let ses_id = ses_message_id();
    let first = pipeline::process_raw_message(&app, &ses_id, &raw, None)
        .await
        .unwrap();
    assert!(first.dropped_reason.is_none(), "{first:?}");
    let ticket_id = first.ticket_id.clone().unwrap();

    let second = pipeline::process_raw_message(&app, &ses_id, &raw, None)
        .await
        .unwrap();
    assert_eq!(
        second.dropped_reason.as_deref(),
        Some("duplicate SES message id")
    );
    assert!(second.ticket_id.is_none());

    // Only one message on the ticket — the duplicate delivery never
    // appended a second one.
    let messages = db.list_ticket_messages(&ticket_id).await.unwrap();
    assert_eq!(messages.len(), 1, "{messages:?}");
}

#[tokio::test]
async fn in_reply_to_threading_appends_to_the_right_ticket() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "irt").await;
    let requester = unique_email("requester");
    let app = test_app(db.clone(), "in-reply-to");

    // First message establishes the thread's rfc_message_id.
    let opening_id = rfc_message_id("opening");
    let first_raw = build_eml(
        &requester,
        &[("To", &support_address), ("Message-ID", &opening_id)],
        "Threaded question",
        "Initial message.",
    );
    let first = pipeline::process_raw_message(&app, &ses_message_id(), &first_raw, None)
        .await
        .unwrap();
    let ticket_id = first.ticket_id.expect("first message opens a ticket");

    // Second message has NO tag at all — only In-Reply-To — and must still
    // land on the same ticket.
    let second_raw = build_eml(
        &requester,
        &[
            ("To", &support_address),
            ("In-Reply-To", &opening_id),
            ("References", &opening_id),
        ],
        "Re: Threaded question",
        "Following up on my question.",
    );
    let second = pipeline::process_raw_message(&app, &ses_message_id(), &second_raw, None)
        .await
        .unwrap();

    assert!(second.dropped_reason.is_none(), "{second:?}");
    assert!(!second.created_new_ticket);
    assert_eq!(second.ticket_id.as_deref(), Some(ticket_id.as_str()));

    let messages = db.list_ticket_messages(&ticket_id).await.unwrap();
    assert_eq!(messages.len(), 2, "{messages:?}");
    let _ = instance;
}

#[tokio::test]
async fn subject_tag_threading_appends_to_the_right_ticket() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "subj").await;
    let requester = unique_email("requester");
    let number = db.increment_ticket_counter(&instance.id).await.unwrap();
    let ticket = db
        .create_ticket(
            &instance.id,
            number,
            "Printer on fire",
            std::slice::from_ref(&requester),
            &[],
        )
        .await
        .unwrap();

    let app = test_app(db.clone(), "subject-tag");
    let tagged_subject = format!("Re: [#{}-{number}] Printer on fire", instance.slug);
    // No tag, no In-Reply-To/References — only the subject tag.
    let raw = build_eml(
        &requester,
        &[("To", &support_address)],
        &tagged_subject,
        "Update: still smoking.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(!outcome.created_new_ticket);
    assert_eq!(outcome.ticket_id.as_deref(), Some(ticket.id.as_str()));
}

#[tokio::test]
async fn attachment_is_stored_and_recorded_on_the_message() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_instance, _domain, support_address) = setup_instance(&db, "attach").await;
    let app = test_app(db.clone(), "attachment");

    let raw = build_eml_with_attachment(
        &unique_email("requester"),
        &support_address,
        "Screenshot attached",
        "See attached.",
        "screenshot.png",
        b"not a real png but bytes are bytes",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert_eq!(outcome.attachment_count, 1);

    let ticket_id = outcome.ticket_id.unwrap();
    let messages = db.list_ticket_messages(&ticket_id).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].attachments.len(), 1);

    // The denormalised flag the ticket list's paperclip reads. Without it a
    // list row has to fetch every message of every ticket on the page just to
    // decide whether to draw an icon.
    let ticket = db
        .get_tickets(&[ticket_id.as_str()])
        .await
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .expect("ticket exists");
    assert!(
        ticket.has_attachments,
        "storing an attachment must mark the ticket"
    );
    let attachment = &messages[0].attachments[0];
    assert_eq!(attachment.filename, "screenshot.png");
    assert!(
        attachment
            .s3_key
            .starts_with(&format!("attachments/{ticket_id}/{}/0/", messages[0].id))
    );
    assert!(attachment.size > 0);
}

#[tokio::test]
async fn wildcard_address_routes_correctly() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            "Wildcard instance",
            &unique_id("wildcard-slug"),
            "Wildcard Support",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .unwrap();
    let domain = format!("{}.toolbox.test", unique_id("wildcard"));
    db.create_inbound_address(
        &format!("*@{domain}"),
        &instance.id,
        db::AddressKind::Wildcard,
    )
    .await
    .unwrap();

    let app = test_app(db.clone(), "wildcard");
    let raw = build_eml(
        &unique_email("requester"),
        &[("To", &format!("anything-goes@{domain}"))],
        "Wildcard-routed ticket",
        "Hello via wildcard.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    let ticket_id = outcome.ticket_id.unwrap();
    let ticket = db
        .get_tickets(&[ticket_id.as_str()])
        .await
        .unwrap()
        .into_iter()
        .next()
        .flatten()
        .unwrap();
    assert_eq!(ticket.instance_id, instance.id);
}

#[tokio::test]
async fn from_our_own_address_is_dropped_as_a_loop() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "loop").await;
    let app = test_app(db.clone(), "loop");

    // From our own inbound address, to our own inbound address — as if an
    // autoresponder or a misconfigured relay bounced our own outbound mail
    // straight back at us.
    let raw = build_eml(
        &support_address,
        &[("To", &support_address)],
        "Bounced back",
        "This should never become a ticket.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();

    assert!(outcome.dropped_reason.is_some(), "{outcome:?}");
    assert!(outcome.ticket_id.is_none());

    let tickets = db
        .list_tickets(
            &instance.id,
            db::TicketListFilter::Visible,
            db::ListTicketsPage {
                after: None,
                before: None,
                limit: 10,
                descending: true,
            },
        )
        .await
        .unwrap();
    assert!(tickets.is_empty(), "{tickets:?}");
}

/// A soft-deleted instance's inbound address still resolves (deleting an
/// instance doesn't touch `inbound_address`), but the message must be
/// dropped the same way mail to no known instance is: logged, no ticket, no
/// mail sent — never silently reopening/creating tickets against an
/// instance nobody is looking at any more. See `db::Handler` step 4 in
/// `inbound::pipeline::process_raw_message` and `SCHEMA.md`'s instance
/// soft-delete section.
#[tokio::test]
async fn mail_to_a_deleted_instance_is_dropped() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "deletedinst").await;
    db.update_instance(&instance.id, db::InstanceUpdateShape::SetDeleted(true))
        .await
        .expect("update_instance");
    let app = test_app(db.clone(), "deletedinst");

    let requester = unique_email("requester");
    let raw = build_eml(
        &requester,
        &[("To", &support_address)],
        "Anybody there?",
        "Hoping someone still reads this inbox.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .expect("process_raw_message");

    assert!(
        outcome.dropped_reason.is_some(),
        "mail to a deleted instance must be dropped: {outcome:?}"
    );
    assert!(outcome.ticket_id.is_none());

    let tickets = db
        .list_tickets(
            &instance.id,
            db::TicketListFilter::Visible,
            db::ListTicketsPage {
                after: None,
                before: None,
                limit: 10,
                descending: true,
            },
        )
        .await
        .unwrap();
    assert!(
        tickets.is_empty(),
        "no ticket must be created against a deleted instance: {tickets:?}"
    );
}

// ── staff notifications ─────────────────────────────────────────────────────
//
// `setup_instance` above creates no members, so every test before this
// point exercises `staff_notify::notify_staff` as a no-op (an empty
// membership list short-circuits before any mail is built) — these two
// give it someone to actually notify.

#[tokio::test]
async fn a_new_ticket_via_email_notifies_members() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "staffnewtick").await;
    let member = db
        .create_user(&unique_email("staffnewtick-member"), "Member")
        .await
        .expect("create_user");
    db.create_membership(&member.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    let app = test_app(db.clone(), "staff-new-ticket");

    let requester = unique_email("requester");
    let raw = build_eml(
        &requester,
        &[("To", &support_address)],
        "Printer on fire",
        "Send help.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .expect("process_raw_message");
    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(outcome.created_new_ticket);

    let sent = app.mail.sent_raw();
    assert_eq!(
        sent.len(),
        1,
        "the member should get exactly one staff notice about the new ticket: {sent:?}"
    );
    assert_eq!(sent[0].to, vec![member.email.clone()]);
    let raw_msg = String::from_utf8_lossy(&sent[0].raw);
    assert!(raw_msg.contains("New ticket from"), "{raw_msg}");
    assert!(
        !raw_msg.contains("+t"),
        "a staff notice must never carry a +t… reply tag: {raw_msg}"
    );
}

#[tokio::test]
async fn a_reply_on_an_existing_ticket_notifies_members_per_their_settings() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _domain, support_address) = setup_instance(&db, "staffreply2").await;
    let requester = unique_email("requester");
    let number = db.increment_ticket_counter(&instance.id).await.unwrap();
    let ticket = db
        .create_ticket(
            &instance.id,
            number,
            "Original subject",
            std::slice::from_ref(&requester),
            &[],
        )
        .await
        .unwrap();

    let opted_in = db
        .create_user(&unique_email("staffreply2-in"), "Opted In")
        .await
        .expect("create_user");
    db.create_membership(&opted_in.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");

    let opted_out = db
        .create_user(&unique_email("staffreply2-out"), "Opted Out")
        .await
        .expect("create_user");
    let opted_out_membership = db
        .create_membership(&opted_out.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    // Turn their (default-on) unassigned_updated setting off — the ticket
    // has no assignee, so without this they'd be notified too.
    db.update_membership_notification_settings(
        &opted_out_membership.id,
        &db::NotificationSettingsPatch {
            unassigned_updated: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect("update_membership_notification_settings");

    let app = test_app(db.clone(), "staff-reply-2");
    let local = support_address.split('@').next().unwrap();
    let domain = support_address.split('@').nth(1).unwrap();
    let tagged_to = format!("{local}+{}@{domain}", reply_tag(&ticket));

    let raw = build_eml(
        &requester,
        &[("To", &tagged_to)],
        "Re: Original subject",
        "Following up.",
    );
    let outcome = pipeline::process_raw_message(&app, &ses_message_id(), &raw, None)
        .await
        .unwrap();
    assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
    assert!(!outcome.created_new_ticket);

    let sent = app.mail.sent_raw();
    assert_eq!(
        sent.len(),
        1,
        "only the opted-in member should be notified, per their own settings: {sent:?}"
    );
    assert_eq!(sent[0].to, vec![opted_in.email.clone()]);
    let raw_msg = String::from_utf8_lossy(&sent[0].raw);
    assert!(raw_msg.contains("replied"), "{raw_msg}");
}
