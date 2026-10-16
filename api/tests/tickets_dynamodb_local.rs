//! Integration tests for the ticket entity, listing GSIs, and mutations
//! against **DynamoDB Local** — not the mocks. `mockdb::Handler` fails every
//! call by design (see its module doc), and the marker-attribute bookkeeping
//! this step is most worried about only shows up against a real table with
//! real GSIs, so this is the one place that can actually prove:
//!
//!   - each of the four listing views (`OPEN`/`CLOSED`/`ALL`/`DELETED`)
//!     returns exactly the right ticket set;
//!   - deleting a ticket drops it out of `OPEN`/`CLOSED`/`ALL` in one write,
//!     `DELETED` still finds it, and restoring reverses all of that;
//!   - assignment writes the `instance_assignee` marker and unassignment
//!     removes it;
//!   - keyset pagination is correct across a real page boundary;
//!   - a member of one instance cannot act on another instance's ticket by id;
//!   - a `Requester` principal never sees an internal note.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test tickets_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** (prints a message, returns) when no reachable local DynamoDB is
//! configured, so `cargo test` and CI stay green with no local stack running.
//! Every row is created fresh per test run (nanoid'd instance/user ids), so
//! repeated runs never collide with each other or with `make local-seed`'s
//! fixture rows.

use std::collections::HashSet;
use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use toolbox::app;
use toolbox::auth::{AuthInfo, Membership};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;
type TestSchema = graphql::ToolboxSchema<TestApp>;

/// See `tests/auth_dynamodb_local.rs`'s identically-named helper for the full
/// rationale.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "tickets_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
             Run `make local-up && make local-tables` first."
        );
        return None;
    }

    Some(prefix)
}

macro_rules! require_local_db {
    () => {
        match local_db_prefix().await {
            Some(prefix) => prefix,
            None => {
                eprintln!(
                    "tickets_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header for how to run it."
                );
                return;
            }
        }
    };
}

fn unique_id(label: &str) -> String {
    format!("{label}-{}", nanoid::nanoid!(8))
}

fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.com", nanoid::nanoid!(8))
}

/// Read `data.<path>` out of a GraphQL response as JSON, panicking with the
/// response's errors (if any) on failure. Mirrors `auth_dynamodb_local.rs`'s
/// identically-named helper.
fn expect_data(response: &Response, path: &str) -> Value {
    assert!(
        response.errors.is_empty(),
        "unexpected GraphQL errors on {path}: {:?}",
        response.errors
    );
    let data = serde_json::to_value(&response.data).expect("response data serializes");
    let mut cur = &data;
    for segment in path.split('.') {
        cur = cur
            .get(segment)
            .unwrap_or_else(|| panic!("response data missing `{segment}` (path {path}): {data}"));
    }
    cur.clone()
}

/// The opposite of `expect_data`: assert the response *did* error, and return
/// the `extensions.code` of the first error so callers can assert on the
/// machine-readable code, not just "something failed".
fn expect_error_code(response: &Response) -> String {
    let err = response
        .errors
        .first()
        .unwrap_or_else(|| panic!("expected a GraphQL error, got none: {response:?}"));
    match err.extensions.as_ref().and_then(|e| e.get("code")) {
        Some(async_graphql::Value::String(s)) => s.clone(),
        other => panic!("error had no string extensions.code: {other:?} ({err:?})"),
    }
}

fn user_auth(user_id: &str, memberships: &[(&str, bool)]) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: memberships
            .iter()
            .map(|(instance_id, is_owner)| Membership {
                instance_id: instance_id.to_string(),
                is_owner: *is_owner,
            })
            .collect(),
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
}

fn requester_auth(email: &str, instance_id: &str) -> AuthInfo {
    AuthInfo::Requester {
        email: email.to_string(),
        instance_id: instance_id.to_string(),
    }
}

/// Create an instance + one owner user + membership, returning
/// `(instance_id, owner_user_id)`. Every test starts from this.
async fn setup_instance(db: &dynamodb::Handler, label: &str) -> (String, String) {
    let instance = db
        .create_instance(
            &unique_id(label),
            &unique_id(&format!("{label}-slug")),
            label,
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    (instance.id, owner.id)
}

/// Create a ticket directly through the DB layer (bypassing `submitTicket`,
/// which only a `Requester` token can call) with an atomic per-instance
/// number, returning the created `db::Ticket`.
async fn make_ticket(db: &dynamodb::Handler, instance_id: &str, subject: &str) -> db::Ticket {
    let number = db
        .increment_ticket_counter(instance_id)
        .await
        .expect("increment_ticket_counter");
    db.create_ticket(
        instance_id,
        number,
        subject,
        &[unique_email("requester")],
        &[],
    )
    .await
    .expect("create_ticket")
}

fn build_app_and_schema(db: dynamodb::Handler) -> (Arc<TestApp>, TestSchema) {
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);
    (my_app, schema)
}

const TICKETS_QUERY: &str = r#"
    query(
        $instanceId: ID!, $status: TicketStatusFilterType, $assignedTo: ID,
        $first: Int, $after: String, $last: Int, $before: String
    ) {
        tickets(
            instanceId: $instanceId, status: $status, assignedTo: $assignedTo,
            first: $first, after: $after, last: $last, before: $before
        ) {
            edges { cursor node { id number status } }
            pageInfo { hasNextPage hasPreviousPage startCursor endCursor }
        }
    }
"#;

const SET_STATUS_MUTATION: &str = r#"
    mutation($ticketId: ID!, $status: TicketStatusType!) {
        setTicketStatus(ticketId: $ticketId, status: $status) { id status }
    }
"#;

const ASSIGN_MUTATION: &str = r#"
    mutation($ticketId: ID!, $userId: ID) {
        assignTicket(ticketId: $ticketId, userId: $userId) { id assigneeUserId }
    }
"#;

const TICKET_WITH_MESSAGES_QUERY: &str = r#"
    query($id: ID!) {
        ticket(id: $id) { id status messages { kind bodyText } }
    }
"#;

/// Ids (in returned order) of every edge in a `tickets` response.
fn edge_ids(tickets_data: &Value) -> Vec<String> {
    tickets_data["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .map(|e| e["node"]["id"].as_str().expect("node.id").to_string())
        .collect()
}

// Mirrors the `tickets` query's own argument list one-for-one, so a params
// struct here would just be a second place to keep in sync with the schema.
#[allow(clippy::too_many_arguments)]
async fn list_tickets(
    schema: &TestSchema,
    instance_id: &str,
    status: &str,
    assigned_to: Option<&str>,
    first: Option<i32>,
    after: Option<&str>,
    last: Option<i32>,
    auth: AuthInfo,
) -> Response {
    let vars = json!({
        "instanceId": instance_id,
        "status": status,
        "assignedTo": assigned_to,
        "first": first,
        "after": after,
        "last": last,
    });
    schema
        .execute(
            Request::new(TICKETS_QUERY)
                .variables(Variables::from_json(vars))
                .data(auth),
        )
        .await
}

async fn set_status(
    schema: &TestSchema,
    ticket_id: &str,
    status: &str,
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(SET_STATUS_MUTATION)
                .variables(Variables::from_json(
                    json!({ "ticketId": ticket_id, "status": status }),
                ))
                .data(auth),
        )
        .await
}

async fn assign(
    schema: &TestSchema,
    ticket_id: &str,
    user_id: Option<&str>,
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(ASSIGN_MUTATION)
                .variables(Variables::from_json(
                    json!({ "ticketId": ticket_id, "userId": user_id }),
                ))
                .data(auth),
        )
        .await
}

#[tokio::test]
async fn the_four_listings_return_exactly_the_right_ticket_set() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_instance(&db, "listings").await;

    let open1 = make_ticket(&db, &instance_id, "open one").await;
    let open2 = make_ticket(&db, &instance_id, "open two").await;
    let to_close = make_ticket(&db, &instance_id, "will be closed").await;
    let now = toolbox::clock::now_sec();
    db.update_ticket(
        &to_close.id,
        db::TicketUpdateShape::SetStatusAndAssignee {
            instance_id: &instance_id,
            status: db::TicketStatus::Closed,
            assignee_user_id: None,
            now,
        },
    )
    .await
    .expect("close ticket");
    let to_delete = make_ticket(&db, &instance_id, "will be deleted").await;
    db.update_ticket(
        &to_delete.id,
        db::TicketUpdateShape::SetStatusAndAssignee {
            instance_id: &instance_id,
            status: db::TicketStatus::Deleted,
            assignee_user_id: None,
            now,
        },
    )
    .await
    .expect("delete ticket");

    let (_my_app, schema) = build_app_and_schema(db);
    let owner_memberships = [(instance_id.as_str(), true)];

    let open_ids: HashSet<String> = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "OPEN",
            None,
            None,
            None,
            None,
            user_auth(&owner_id, &owner_memberships),
        )
        .await,
        "tickets",
    ))
    .into_iter()
    .collect();
    assert_eq!(
        open_ids,
        [open1.id.clone(), open2.id.clone()].into_iter().collect(),
        "OPEN must return exactly the two open tickets"
    );

    let closed_ids: HashSet<String> = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "CLOSED",
            None,
            None,
            None,
            None,
            user_auth(&owner_id, &owner_memberships),
        )
        .await,
        "tickets",
    ))
    .into_iter()
    .collect();
    assert_eq!(
        closed_ids,
        [to_close.id.clone()].into_iter().collect(),
        "CLOSED must return exactly the closed ticket"
    );

    let all_ids: HashSet<String> = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "ALL",
            None,
            None,
            None,
            None,
            user_auth(&owner_id, &owner_memberships),
        )
        .await,
        "tickets",
    ))
    .into_iter()
    .collect();
    assert_eq!(
        all_ids,
        [open1.id.clone(), open2.id.clone(), to_close.id.clone()]
            .into_iter()
            .collect(),
        "ALL must return every non-deleted ticket, and never the deleted one"
    );

    let deleted_ids: HashSet<String> = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "DELETED",
            None,
            None,
            None,
            None,
            user_auth(&owner_id, &owner_memberships),
        )
        .await,
        "tickets",
    ))
    .into_iter()
    .collect();
    assert_eq!(
        deleted_ids,
        [to_delete.id.clone()].into_iter().collect(),
        "DELETED must return exactly the deleted ticket"
    );
}

#[tokio::test]
async fn deleted_status_filter_is_owner_only() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_instance(&db, "ownercheck").await;
    let agent_id = db
        .create_user(&unique_email("agent"), "Agent")
        .await
        .expect("create_user")
        .id;
    db.create_membership(&agent_id, &instance_id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    make_ticket(&db, &instance_id, "irrelevant").await;

    let (_my_app, schema) = build_app_and_schema(db);

    let owner_response = list_tickets(
        &schema,
        &instance_id,
        "DELETED",
        None,
        None,
        None,
        None,
        user_auth(&owner_id, &[(&instance_id, true)]),
    )
    .await;
    assert!(
        owner_response.errors.is_empty(),
        "an owner must be able to list DELETED: {:?}",
        owner_response.errors
    );

    let agent_response = list_tickets(
        &schema,
        &instance_id,
        "DELETED",
        None,
        None,
        None,
        None,
        user_auth(&agent_id, &[(&instance_id, false)]),
    )
    .await;
    assert_eq!(expect_error_code(&agent_response), "FORBIDDEN");
}

#[tokio::test]
async fn delete_drops_a_ticket_from_open_closed_all_in_one_write_and_restore_reverses_it() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_instance(&db, "deleterestore").await;
    let ticket = make_ticket(&db, &instance_id, "delete me").await;

    let (_my_app, schema) = build_app_and_schema(db);
    let memberships = [(instance_id.as_str(), true)];

    // Before delete: visible in OPEN and ALL.
    for status in ["OPEN", "ALL"] {
        let ids = edge_ids(&expect_data(
            &list_tickets(
                &schema,
                &instance_id,
                status,
                None,
                None,
                None,
                None,
                user_auth(&owner_id, &memberships),
            )
            .await,
            "tickets",
        ));
        assert!(
            ids.contains(&ticket.id),
            "must appear under {status} before delete"
        );
    }

    // setTicketStatus(DELETED) — one write.
    let response = set_status(
        &schema,
        &ticket.id,
        "DELETED",
        user_auth(&owner_id, &memberships),
    )
    .await;
    assert_eq!(
        expect_data(&response, "setTicketStatus.status"),
        json!("DELETED")
    );

    // After delete: gone from OPEN/CLOSED/ALL in one write, present under DELETED.
    for status in ["OPEN", "CLOSED", "ALL"] {
        let ids = edge_ids(&expect_data(
            &list_tickets(
                &schema,
                &instance_id,
                status,
                None,
                None,
                None,
                None,
                user_auth(&owner_id, &memberships),
            )
            .await,
            "tickets",
        ));
        assert!(
            !ids.contains(&ticket.id),
            "deleted ticket must not appear under {status}"
        );
    }
    let deleted_ids = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "DELETED",
            None,
            None,
            None,
            None,
            user_auth(&owner_id, &memberships),
        )
        .await,
        "tickets",
    ));
    assert!(
        deleted_ids.contains(&ticket.id),
        "deleted ticket must still be findable via status: DELETED"
    );

    // Restore via setTicketStatus(OPEN) — reverses it.
    let response = set_status(
        &schema,
        &ticket.id,
        "OPEN",
        user_auth(&owner_id, &memberships),
    )
    .await;
    assert_eq!(
        expect_data(&response, "setTicketStatus.status"),
        json!("OPEN")
    );

    for status in ["OPEN", "ALL"] {
        let ids = edge_ids(&expect_data(
            &list_tickets(
                &schema,
                &instance_id,
                status,
                None,
                None,
                None,
                None,
                user_auth(&owner_id, &memberships),
            )
            .await,
            "tickets",
        ));
        assert!(
            ids.contains(&ticket.id),
            "must reappear under {status} after restore"
        );
    }
    let deleted_ids = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "DELETED",
            None,
            None,
            None,
            None,
            user_auth(&owner_id, &memberships),
        )
        .await,
        "tickets",
    ));
    assert!(
        !deleted_ids.contains(&ticket.id),
        "restored ticket must no longer appear under DELETED"
    );
}

#[tokio::test]
async fn assignment_maintains_the_assignee_marker_and_unassignment_removes_it() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_instance(&db, "assign").await;
    let agent = db
        .create_user(&unique_email("agent"), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance_id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let ticket = make_ticket(&db, &instance_id, "assign me").await;

    let (_my_app, schema) = build_app_and_schema(db);
    let memberships = [(instance_id.as_str(), true)];

    // Not assigned yet: absent from "assigned to me".
    let ids = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "OPEN",
            Some(&agent.id),
            None,
            None,
            None,
            user_auth(&owner_id, &memberships),
        )
        .await,
        "tickets",
    ));
    assert!(!ids.contains(&ticket.id));

    let response = assign(
        &schema,
        &ticket.id,
        Some(&agent.id),
        user_auth(&owner_id, &memberships),
    )
    .await;
    assert_eq!(
        expect_data(&response, "assignTicket.assigneeUserId"),
        json!(agent.id)
    );

    let ids = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "OPEN",
            Some(&agent.id),
            None,
            None,
            None,
            user_auth(&owner_id, &memberships),
        )
        .await,
        "tickets",
    ));
    assert!(
        ids.contains(&ticket.id),
        "instance_assignee marker must make the ticket findable via assignedTo"
    );

    // Unassign (userId: null) — marker must be removed.
    let response = assign(
        &schema,
        &ticket.id,
        None,
        user_auth(&owner_id, &memberships),
    )
    .await;
    assert_eq!(
        expect_data(&response, "assignTicket.assigneeUserId"),
        json!(null)
    );

    let ids = edge_ids(&expect_data(
        &list_tickets(
            &schema,
            &instance_id,
            "OPEN",
            Some(&agent.id),
            None,
            None,
            None,
            user_auth(&owner_id, &memberships),
        )
        .await,
        "tickets",
    ));
    assert!(
        !ids.contains(&ticket.id),
        "unassigning must remove the instance_assignee marker"
    );
}

#[tokio::test]
async fn pagination_is_correct_across_a_page_boundary() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_instance(&db, "paging").await;

    const TOTAL: usize = 37;
    const PAGE: i32 = 10;
    let mut created_ids = HashSet::new();
    for i in 0..TOTAL {
        let t = make_ticket(&db, &instance_id, &format!("paged ticket {i}")).await;
        created_ids.insert(t.id);
    }

    let (_my_app, schema) = build_app_and_schema(db);
    let memberships = [(instance_id.as_str(), true)];

    // Forward pagination: walk every page with `first`/`after` until
    // hasNextPage is false, collecting every id and asserting no duplicates.
    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    let mut page_count = 0usize;
    loop {
        let response = list_tickets(
            &schema,
            &instance_id,
            "OPEN",
            None,
            Some(PAGE),
            after.as_deref(),
            None,
            user_auth(&owner_id, &memberships),
        )
        .await;
        let data = expect_data(&response, "tickets");
        let page_ids = edge_ids(&data);
        page_count += 1;
        for id in &page_ids {
            assert!(
                !seen.contains(id),
                "pagination must never repeat a ticket across pages"
            );
        }
        seen.extend(page_ids);

        let has_next = data["pageInfo"]["hasNextPage"].as_bool().unwrap();
        if !has_next {
            break;
        }
        after = Some(
            data["pageInfo"]["endCursor"]
                .as_str()
                .expect("endCursor present when hasNextPage")
                .to_string(),
        );
    }

    assert_eq!(
        seen.len(),
        TOTAL,
        "paging through every page must visit every ticket exactly once"
    );
    let seen_set: HashSet<String> = seen.into_iter().collect();
    assert_eq!(
        seen_set, created_ids,
        "the paged-through set must match what was created"
    );

    // 37 rows over pages of 10 must take at least 4 pages — otherwise this
    // test isn't actually exercising a page boundary.
    assert!(
        page_count >= 4,
        "test setup sanity: expected at least 4 pages, walked {page_count}"
    );

    // Backward pagination from the end (`last`) must land on a different page
    // than forward pagination's first page, and report the opposite
    // hasNextPage/hasPreviousPage boundary flags.
    let first_page_response = list_tickets(
        &schema,
        &instance_id,
        "OPEN",
        None,
        Some(PAGE),
        None,
        None,
        user_auth(&owner_id, &memberships),
    )
    .await;
    let first_page = expect_data(&first_page_response, "tickets");
    assert!(first_page["pageInfo"]["hasNextPage"].as_bool().unwrap());
    assert!(!first_page["pageInfo"]["hasPreviousPage"].as_bool().unwrap());

    let last_page_response = schema
        .execute(
            Request::new(TICKETS_QUERY)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id, "status": "OPEN", "last": PAGE,
                })))
                .data(user_auth(&owner_id, &memberships)),
        )
        .await;
    let last_page = expect_data(&last_page_response, "tickets");
    assert!(last_page["pageInfo"]["hasPreviousPage"].as_bool().unwrap());
    assert!(!last_page["pageInfo"]["hasNextPage"].as_bool().unwrap());
    assert_ne!(
        edge_ids(&first_page),
        edge_ids(&last_page),
        "the first and last pages of 37 rows over pages of 10 must differ"
    );
}

#[tokio::test]
async fn a_member_of_one_instance_cannot_act_on_another_instances_ticket_by_id() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_a, owner_a) = setup_instance(&db, "crossa").await;
    let (instance_b, _owner_b) = setup_instance(&db, "crossb").await;
    let ticket_b = make_ticket(&db, &instance_b, "belongs to B").await;

    let (_my_app, schema) = build_app_and_schema(db);
    let a_memberships = [(instance_a.as_str(), true)];

    let response = set_status(
        &schema,
        &ticket_b.id,
        "CLOSED",
        user_auth(&owner_a, &a_memberships),
    )
    .await;
    assert_eq!(
        expect_error_code(&response),
        "NOT_FOUND",
        "a member of instance A must not be able to act on instance B's ticket by id"
    );

    let response = assign(
        &schema,
        &ticket_b.id,
        Some(&owner_a),
        user_auth(&owner_a, &a_memberships),
    )
    .await;
    assert_eq!(expect_error_code(&response), "NOT_FOUND");

    // A cannot even read B's ticket by id — confirms the guard applies to
    // reads, not only writes.
    let ticket_after = schema
        .execute(
            Request::new(TICKET_WITH_MESSAGES_QUERY)
                .variables(Variables::from_json(json!({ "id": ticket_b.id })))
                .data(user_auth(&owner_a, &a_memberships)),
        )
        .await;
    assert_eq!(expect_data(&ticket_after, "ticket"), json!(null));
}

#[tokio::test]
async fn a_requester_principal_cannot_see_internal_notes() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_instance(&db, "notes").await;
    let requester_email = unique_email("requester");

    let number = db
        .increment_ticket_counter(&instance_id)
        .await
        .expect("increment_ticket_counter");
    let ticket = db
        .create_ticket(
            &instance_id,
            number,
            "needs help",
            std::slice::from_ref(&requester_email),
            &[],
        )
        .await
        .expect("create_ticket");
    db.create_ticket_message(
        &ticket.id,
        db::TicketMessageKind::Inbound,
        None,
        Some(&requester_email),
        &[],
        &[],
        Some("Something is broken"),
        None,
        None,
        None,
    )
    .await
    .expect("create initial message");
    db.create_ticket_message(
        &ticket.id,
        db::TicketMessageKind::Note,
        Some(&owner_id),
        None,
        &[],
        &[],
        Some("Internal-only: this is a known issue, waiting on vendor fix"),
        None,
        None,
        None,
    )
    .await
    .expect("create internal note");

    let (_my_app, schema) = build_app_and_schema(db);

    // The owner sees both messages, including the note.
    let owner_response = schema
        .execute(
            Request::new(TICKET_WITH_MESSAGES_QUERY)
                .variables(Variables::from_json(json!({ "id": ticket.id })))
                .data(user_auth(&owner_id, &[(instance_id.as_str(), true)])),
        )
        .await;
    let owner_messages = expect_data(&owner_response, "ticket.messages");
    let owner_kinds: Vec<String> = owner_messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(
        owner_kinds.contains(&"NOTE".to_string()),
        "a member must see the internal note: {owner_kinds:?}"
    );
    assert_eq!(owner_messages.as_array().unwrap().len(), 2);

    // The requester who opened this exact ticket sees the thread, but never
    // the note.
    let requester_response = schema
        .execute(
            Request::new(TICKET_WITH_MESSAGES_QUERY)
                .variables(Variables::from_json(json!({ "id": ticket.id })))
                .data(requester_auth(&requester_email, &instance_id)),
        )
        .await;
    let requester_messages = expect_data(&requester_response, "ticket.messages");
    let requester_kinds: Vec<String> = requester_messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !requester_kinds.contains(&"NOTE".to_string()),
        "a requester principal must never see an internal note: {requester_kinds:?}"
    );
    assert_eq!(
        requester_messages.as_array().unwrap().len(),
        1,
        "the requester must still see their own non-note message"
    );

    // A requester scoped to a *different* email must not be able to read
    // this ticket at all — the visibility check is by email, not just
    // instance.
    let stranger_response = schema
        .execute(
            Request::new(TICKET_WITH_MESSAGES_QUERY)
                .variables(Variables::from_json(json!({ "id": ticket.id })))
                .data(requester_auth(&unique_email("stranger"), &instance_id)),
        )
        .await;
    assert_eq!(expect_data(&stranger_response, "ticket"), json!(null));
}
