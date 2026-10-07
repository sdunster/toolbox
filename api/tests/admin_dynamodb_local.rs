//! Integration tests for the superuser-only admin GraphQL surface —
//! `createInstance`/`updateInstance`/`setInstanceDeleted`,
//! `createUser`/`updateUser`/`deleteUser`,
//! `addMember`/`removeMember`/`setMemberRole` — against **DynamoDB Local**,
//! through the real schema (not the pure guard-truth-table unit tests in
//! `src/graphql/auth.rs`, which never touch a database). Also covers the two
//! deleted-instance-visibility fixes this feature makes:
//! `User.memberships` filtering out a deleted instance, and
//! `Query.instance(slug)` returning `null` for one.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test admin_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured, so `cargo test`
//! and CI stay green with no local stack running. Every row is created fresh
//! per test run (nanoid'd ids/slugs/emails), so repeated runs never collide.

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
use toolbox::mockstorage;

type TestApp = app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage>;
type TestSchema = graphql::ToolboxSchema<TestApp>;

/// See `tests/auth_dynamodb_local.rs`'s identically-named helper for the full
/// rationale.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "admin_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
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
                    "admin_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header for how to run it."
                );
                return;
            }
        }
    };
}

/// Slug-safe: `db::validate_slug` rejects anything but lowercase
/// letters/digits/hyphens, but `nanoid::nanoid!`'s default alphabet
/// includes `_` — filtered out here so a slug built from this never
/// intermittently fails format validation.
fn unique_id(label: &str) -> String {
    let suffix: String = nanoid::nanoid!(16)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("{label}-{suffix}").to_lowercase()
}

/// Lowercased deliberately: `createUser`/`updateUser` normalize the `email`
/// argument (`db::normalize_user_email`), so a mixed-case nanoid here would
/// only be asserting against that normalization when a test creates a user
/// directly via `db.create_user` and later looks it up through the
/// normalized GraphQL path.
fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.com", nanoid::nanoid!(8)).to_lowercase()
}

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

/// Mirrors `tickets_dynamodb_local.rs`'s identically-named helper.
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

fn superuser_auth(user_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
        is_superuser: true,
        token_id: None,
        grant_id: None,
    }
}

/// An ordinary member — not a superuser, and (per the superuser boundary)
/// membership in an instance grants nothing on the admin surface either.
fn member_auth(user_id: &str, instance_id: &str, is_owner: bool) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![Membership {
            instance_id: instance_id.to_string(),
            is_owner,
        }],
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
}

fn build_app_and_schema(db: dynamodb::Handler) -> (Arc<TestApp>, TestSchema) {
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);
    (my_app, schema)
}

const CREATE_INSTANCE_MUTATION: &str = r#"
    mutation($name: String!, $slug: String!, $publicSubmissionEnabled: Boolean!) {
        createInstance(name: $name, slug: $slug, publicSubmissionEnabled: $publicSubmissionEnabled) {
            id name slug fromName deleted
        }
    }
"#;

#[tokio::test]
async fn create_instance_requires_superuser() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_my_app, schema) = build_app_and_schema(db);

    // No credentials at all.
    let anon = schema
        .execute(
            Request::new(CREATE_INSTANCE_MUTATION).variables(Variables::from_json(json!({
                "name": "Acme", "slug": unique_id("acme"), "publicSubmissionEnabled": false,
            }))),
        )
        .await;
    assert_eq!(expect_error_code(&anon), "UNAUTHENTICATED");

    // An owner of some other instance is still not a superuser — the
    // superuser boundary is disjoint from Member/InstanceOwner.
    let owner = schema
        .execute(
            Request::new(CREATE_INSTANCE_MUTATION)
                .variables(Variables::from_json(json!({
                    "name": "Acme", "slug": unique_id("acme"), "publicSubmissionEnabled": false,
                })))
                .data(member_auth("u1", "some-instance", true)),
        )
        .await;
    assert_eq!(expect_error_code(&owner), "UNAUTHENTICATED");
}

#[tokio::test]
async fn create_instance_rejects_an_already_taken_slug() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let slug = unique_id("taken-slug");
    db.create_instance(
        "Existing",
        &slug,
        "Existing",
        "",
        false,
        db::InstanceKind::Support,
    )
    .await
    .expect("create_instance");
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(CREATE_INSTANCE_MUTATION)
                .variables(Variables::from_json(json!({
                    "name": "New Co", "slug": slug, "publicSubmissionEnabled": false,
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&response), "CONFLICT");
}

#[tokio::test]
async fn create_instance_rejects_an_invalid_slug_and_defaults_from_name() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_my_app, schema) = build_app_and_schema(db);

    let bad = schema
        .execute(
            Request::new(CREATE_INSTANCE_MUTATION)
                .variables(Variables::from_json(json!({
                    "name": "Acme", "slug": "Not A Valid Slug!", "publicSubmissionEnabled": false,
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert!(!bad.errors.is_empty(), "expected an invalid-slug error");

    let good_slug = unique_id("acme");
    let ok = schema
        .execute(
            Request::new(CREATE_INSTANCE_MUTATION)
                .variables(Variables::from_json(json!({
                    "name": "Acme Co", "slug": good_slug, "publicSubmissionEnabled": true,
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    let created = expect_data(&ok, "createInstance");
    assert_eq!(created["name"], "Acme Co");
    assert_eq!(
        created["fromName"], "Acme Co",
        "fromName must default to name"
    );
    assert_eq!(created["deleted"], false);
}

const SET_INSTANCE_DELETED_MUTATION: &str = r#"
    mutation($id: ID!, $deleted: Boolean!) {
        setInstanceDeleted(id: $id, deleted: $deleted) { id deleted }
    }
"#;

const INSTANCE_BY_SLUG_QUERY: &str = r#"
    query($slug: String!) { instance(slug: $slug) { id } }
"#;

const ME_MEMBERSHIPS_QUERY: &str = r#"
    query { me { memberships { instance { id } } } }
"#;

#[tokio::test]
async fn deleting_an_instance_hides_it_from_slug_resolution_and_member_memberships() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let slug = unique_id("vanish");
    let instance = db
        .create_instance(
            "Vanish Co",
            &slug,
            "Vanish Co",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let member = db
        .create_user(&unique_email("member"), "Member")
        .await
        .expect("create_user");
    db.create_membership(&member.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let (_my_app, schema) = build_app_and_schema(db);

    // Before deletion: resolves, and shows up in the member's memberships.
    let before = schema
        .execute(
            Request::new(INSTANCE_BY_SLUG_QUERY)
                .variables(Variables::from_json(json!({"slug": slug})))
                .data(member_auth(&member.id, &instance.id, false)),
        )
        .await;
    assert!(!expect_data(&before, "instance").is_null());

    let before_memberships = schema
        .execute(Request::new(ME_MEMBERSHIPS_QUERY).data(member_auth(
            &member.id,
            &instance.id,
            false,
        )))
        .await;
    let memberships = expect_data(&before_memberships, "me.memberships");
    assert_eq!(memberships.as_array().map(|a| a.len()), Some(1));

    // Soft-delete via the superuser-only mutation.
    let deleted = schema
        .execute(
            Request::new(SET_INSTANCE_DELETED_MUTATION)
                .variables(Variables::from_json(
                    json!({"id": instance.id, "deleted": true}),
                ))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_data(&deleted, "setInstanceDeleted.deleted"), true);

    // After deletion: instance(slug) returns null (never an error — no
    // probing), and it drops out of the member's memberships.
    let after = schema
        .execute(
            Request::new(INSTANCE_BY_SLUG_QUERY)
                .variables(Variables::from_json(json!({"slug": slug})))
                .data(member_auth(&member.id, &instance.id, false)),
        )
        .await;
    assert!(
        after.errors.is_empty(),
        "must not error: {:?}",
        after.errors
    );
    assert!(expect_data(&after, "instance").is_null());

    let after_memberships = schema
        .execute(Request::new(ME_MEMBERSHIPS_QUERY).data(member_auth(
            &member.id,
            &instance.id,
            false,
        )))
        .await;
    let memberships = expect_data(&after_memberships, "me.memberships");
    assert_eq!(
        memberships.as_array().map(|a| a.len()),
        Some(0),
        "a deleted instance must be filtered out of User.memberships"
    );

    // Restoring brings it back.
    let restored = schema
        .execute(
            Request::new(SET_INSTANCE_DELETED_MUTATION)
                .variables(Variables::from_json(
                    json!({"id": instance.id, "deleted": false}),
                ))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_data(&restored, "setInstanceDeleted.deleted"), false);
    let restored_slug = schema
        .execute(
            Request::new(INSTANCE_BY_SLUG_QUERY)
                .variables(Variables::from_json(json!({"slug": slug})))
                .data(member_auth(&member.id, &instance.id, false)),
        )
        .await;
    assert!(!expect_data(&restored_slug, "instance").is_null());
}

const INSTANCE_MEMBERS_QUERY: &str = r#"
    query($slug: String!) { instance(slug: $slug) { members { role user { id name } } } }
"#;

const INSTANCE_MEMBERS_WITH_THEIR_MEMBERSHIPS_QUERY: &str = r#"
    query($slug: String!) {
        instance(slug: $slug) { members { user { id memberships { instance { id } } } } }
    }
"#;

/// Any member — an agent as well as an owner — can list their instance's
/// members (agents hand tickets to each other), but a colleague's `User`
/// reached that way never reveals which *other* instances they belong to.
#[tokio::test]
async fn an_agent_can_list_members_but_not_a_colleagues_memberships() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let slug = unique_id("team");
    let instance = db
        .create_instance(
            "Team Co",
            &slug,
            "Team Co",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let other = db
        .create_instance(
            "Elsewhere Co",
            &unique_id("elsewhere"),
            "Elsewhere Co",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let agent = db
        .create_user(&unique_email("agent"), "Agent")
        .await
        .expect("create_user");
    let owner = db
        .create_user(&unique_email("owner"), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    db.create_membership(&owner.id, &other.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    let outsider = db
        .create_user(&unique_email("outsider"), "Outsider")
        .await
        .expect("create_user");
    db.create_membership(&outsider.id, &other.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    let (_my_app, schema) = build_app_and_schema(db);

    let listed = schema
        .execute(
            Request::new(INSTANCE_MEMBERS_QUERY)
                .variables(Variables::from_json(json!({"slug": slug})))
                .data(member_auth(&agent.id, &instance.id, false)),
        )
        .await;
    let members = expect_data(&listed, "instance.members");
    let ids: Vec<&str> = members
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["user"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{members:?}");
    assert!(ids.contains(&agent.id.as_str()) && ids.contains(&owner.id.as_str()));

    // The colleague's other instance stays hidden: FORBIDDEN, not an empty
    // list that would look like "no other memberships".
    let probed = schema
        .execute(
            Request::new(INSTANCE_MEMBERS_WITH_THEIR_MEMBERSHIPS_QUERY)
                .variables(Variables::from_json(json!({"slug": slug})))
                .data(member_auth(&agent.id, &instance.id, false)),
        )
        .await;
    assert_eq!(expect_error_code(&probed), "FORBIDDEN");

    // A member of some other instance gets nothing: `instance(slug)` is null
    // for them.
    let outside = schema
        .execute(
            Request::new(INSTANCE_MEMBERS_QUERY)
                .variables(Variables::from_json(json!({"slug": slug})))
                .data(member_auth(&outsider.id, &other.id, true)),
        )
        .await;
    assert!(expect_data(&outside, "instance").is_null());
}

const CREATE_USER_MUTATION: &str = r#"
    mutation($email: String!, $name: String!) {
        createUser(email: $email, name: $name) { id email isSuperuser }
    }
"#;

#[tokio::test]
async fn create_user_rejects_an_already_taken_email() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let email = unique_email("taken");
    db.create_user(&email, "Existing")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(CREATE_USER_MUTATION)
                .variables(Variables::from_json(json!({"email": email, "name": "New"})))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&response), "CONFLICT");
}

#[tokio::test]
async fn create_user_is_never_a_superuser_by_default() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(CREATE_USER_MUTATION)
                .variables(Variables::from_json(json!({
                    "email": unique_email("plain"), "name": "Plain",
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_data(&response, "createUser.isSuperuser"), false);
}

/// `createUser` normalizes (`db::normalize_user_email`): a mixed-case,
/// space-padded email is stored trimmed and lowercase, and a second
/// `createUser` with a differently-cased variant of the same address is
/// rejected as a conflict against the first.
#[tokio::test]
async fn create_user_normalizes_email_and_rejects_a_case_variant() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_my_app, schema) = build_app_and_schema(db);

    let suffix = nanoid::nanoid!(8).to_lowercase();
    let local = format!("bob-{suffix}");
    let expected_email = format!("{local}@example.com");
    // Capitalize the leading letter and shout the domain, wrapped in
    // whitespace — mixed case *and* untrimmed, both of which
    // `db::normalize_user_email` must undo.
    let mixed_case = format!("  Bob-{suffix}@Example.COM ");
    assert_ne!(
        mixed_case.trim().to_lowercase(),
        mixed_case,
        "fixture must actually exercise mixed case/whitespace"
    );

    let created = schema
        .execute(
            Request::new(CREATE_USER_MUTATION)
                .variables(Variables::from_json(json!({
                    "email": mixed_case, "name": "Bob",
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_data(&created, "createUser.email"), expected_email);

    // A second createUser with yet another case variant of the same address
    // must be rejected as a conflict against the first, now-lowercase row.
    let other_case_variant = format!("{}@example.com", local.to_uppercase());
    let conflict = schema
        .execute(
            Request::new(CREATE_USER_MUTATION)
                .variables(Variables::from_json(json!({
                    "email": other_case_variant, "name": "Also Bob",
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&conflict), "CONFLICT");
}

const UPDATE_USER_MUTATION: &str = r#"
    mutation($id: ID!, $name: String!, $email: String!, $enabled: Boolean!) {
        updateUser(id: $id, name: $name, email: $email, enabled: $enabled) {
            id name email enabled
        }
    }
"#;

const DELETE_USER_MUTATION: &str = r#"
    mutation($id: ID!) { deleteUser(id: $id) { id enabled } }
"#;

#[tokio::test]
async fn update_user_rejects_an_email_already_taken_by_someone_else() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let taken_email = unique_email("taken-target");
    db.create_user(&taken_email, "Other")
        .await
        .expect("create_user");
    let user = db
        .create_user(&unique_email("mover"), "Mover")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(UPDATE_USER_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": user.id, "name": "Mover", "email": taken_email, "enabled": true,
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&response), "CONFLICT");
}

/// Same conflict, but the incoming email only *differs in case* from the
/// other user's already-lowercase stored email — `updateUser` must still
/// normalize before comparing, so this can't slip through as "a different
/// address" merely because the letters differ in case.
#[tokio::test]
async fn update_user_rejects_a_case_variant_of_someone_elses_email() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let taken_email = unique_email("taken-target-case");
    db.create_user(&taken_email, "Other")
        .await
        .expect("create_user");
    let user = db
        .create_user(&unique_email("mover-case"), "Mover")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db);

    let case_variant = taken_email.to_uppercase();
    let response = schema
        .execute(
            Request::new(UPDATE_USER_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": user.id, "name": "Mover", "email": case_variant, "enabled": true,
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&response), "CONFLICT");
}

#[tokio::test]
async fn update_user_rejects_disabling_the_callers_own_account() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let caller = db
        .create_user(&unique_email("self-lockout"), "Self")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(UPDATE_USER_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": caller.id, "name": "Self", "email": caller.email, "enabled": false,
                })))
                .data(superuser_auth(&caller.id)),
        )
        .await;
    assert_eq!(expect_error_code(&response), "FORBIDDEN");
}

#[tokio::test]
async fn delete_user_rejects_the_callers_own_account() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let caller = db
        .create_user(&unique_email("self-delete"), "Self")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(DELETE_USER_MUTATION)
                .variables(Variables::from_json(json!({"id": caller.id})))
                .data(superuser_auth(&caller.id)),
        )
        .await;
    assert_eq!(expect_error_code(&response), "FORBIDDEN");
}

#[tokio::test]
async fn delete_user_disables_and_removes_every_membership() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let target = db
        .create_user(&unique_email("doomed"), "Doomed")
        .await
        .expect("create_user");
    let inst_a = db
        .create_instance(
            "A",
            &unique_id("a"),
            "A",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let inst_b = db
        .create_instance(
            "B",
            &unique_id("b"),
            "B",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    db.create_membership(&target.id, &inst_a.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    db.create_membership(&target.id, &inst_b.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    let (_my_app, schema) = build_app_and_schema(db.clone());

    let response = schema
        .execute(
            Request::new(DELETE_USER_MUTATION)
                .variables(Variables::from_json(json!({"id": target.id})))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_data(&response, "deleteUser.enabled"), false);

    let stored = db
        .get_users(&[target.id.as_str()])
        .await
        .expect("get_users")
        .into_iter()
        .next()
        .flatten()
        .expect("user still exists (soft delete, not hard delete)");
    assert!(!stored.enabled);

    let memberships = db
        .list_memberships_by_user(&target.id)
        .await
        .expect("list_memberships_by_user");
    assert!(
        memberships.is_empty(),
        "every membership must be removed: {memberships:?}"
    );

    // Retrying after the fact (simulating a retry of a partially-failed
    // call) must converge cleanly, not error on an "already disabled"
    // precondition.
    let retry = schema
        .execute(
            Request::new(DELETE_USER_MUTATION)
                .variables(Variables::from_json(json!({"id": target.id})))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_data(&retry, "deleteUser.enabled"), false);
}

const ADD_MEMBER_MUTATION: &str = r#"
    mutation($instanceId: ID!, $userId: ID!, $role: MembershipRoleType!) {
        addMember(instanceId: $instanceId, userId: $userId, role: $role) { id }
    }
"#;

const SET_MEMBER_ROLE_MUTATION: &str = r#"
    mutation($instanceId: ID!, $userId: ID!, $role: MembershipRoleType!) {
        setMemberRole(instanceId: $instanceId, userId: $userId, role: $role) { id }
    }
"#;

const REMOVE_MEMBER_MUTATION: &str = r#"
    mutation($instanceId: ID!, $userId: ID!) {
        removeMember(instanceId: $instanceId, userId: $userId) { id }
    }
"#;

#[tokio::test]
async fn add_member_then_change_role_then_remove() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            "Membership Co",
            &unique_id("membership-co"),
            "M",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let user = db
        .create_user(&unique_email("newmember"), "New Member")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db.clone());

    let added = schema
        .execute(
            Request::new(ADD_MEMBER_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id, "userId": user.id, "role": "AGENT",
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert!(added.errors.is_empty(), "{:?}", added.errors);

    // Adding again must be rejected (already a member).
    let dup = schema
        .execute(
            Request::new(ADD_MEMBER_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id, "userId": user.id, "role": "AGENT",
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&dup), "CONFLICT");

    let memberships = db
        .list_memberships_by_user(&user.id)
        .await
        .expect("list_memberships_by_user");
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].role, db::MembershipRole::Agent);

    let role_changed = schema
        .execute(
            Request::new(SET_MEMBER_ROLE_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id, "userId": user.id, "role": "OWNER",
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert!(role_changed.errors.is_empty(), "{:?}", role_changed.errors);

    let memberships = db
        .list_memberships_by_user(&user.id)
        .await
        .expect("list_memberships_by_user");
    assert_eq!(
        memberships.len(),
        1,
        "role change must not create a second row"
    );
    assert_eq!(memberships[0].role, db::MembershipRole::Owner);

    let removed = schema
        .execute(
            Request::new(REMOVE_MEMBER_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id, "userId": user.id,
                })))
                .data(superuser_auth("su1")),
        )
        .await;
    assert!(removed.errors.is_empty(), "{:?}", removed.errors);
    let memberships = db
        .list_memberships_by_user(&user.id)
        .await
        .expect("list_memberships_by_user");
    assert!(memberships.is_empty());
}

const ADMIN_USER_PASSKEYS_QUERY: &str = r#"
    query($id: ID!) { adminUser(id: $id) { id passkeys { id } } }
"#;

const ME_PASSKEYS_QUERY: &str = r#"
    query { me { passkeys { id } } }
"#;

#[tokio::test]
async fn passkeys_are_forbidden_for_anyone_but_the_user_themselves() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let target = db
        .create_user(&unique_email("haspasskeys"), "Has Passkeys")
        .await
        .expect("create_user");
    let (_my_app, schema) = build_app_and_schema(db);

    // A *different* superuser looking up someone else's passkeys — even via
    // the legitimate adminUser query, which only requires the caller (not
    // the target) to be a superuser — must be refused.
    let as_superuser = schema
        .execute(
            Request::new(ADMIN_USER_PASSKEYS_QUERY)
                .variables(Variables::from_json(json!({"id": target.id})))
                .data(superuser_auth("su1")),
        )
        .await;
    assert_eq!(expect_error_code(&as_superuser), "FORBIDDEN");

    // The user themselves can still read their own (empty) list, via `me`
    // (they are not a superuser, so `adminUser` itself would reject them at
    // the guard before `passkeys` is ever reached).
    let as_self = schema
        .execute(Request::new(ME_PASSKEYS_QUERY).data(AuthInfo::User {
            id: target.id.clone(),
            memberships: vec![],
            is_superuser: false,
            token_id: None,
            grant_id: None,
        }))
        .await;
    assert!(as_self.errors.is_empty(), "{:?}", as_self.errors);
    assert_eq!(
        expect_data(&as_self, "me.passkeys")
            .as_array()
            .map(Vec::len),
        Some(0)
    );
}

const ADMIN_INSTANCES_QUERY: &str = r#"
    query { adminInstances { id deleted } }
"#;

#[tokio::test]
async fn admin_instances_includes_deleted_ones_and_requires_superuser() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            "Listed",
            &unique_id("listed"),
            "Listed",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    db.update_instance(&instance.id, db::InstanceUpdateShape::SetDeleted(true))
        .await
        .expect("update_instance");
    let (_my_app, schema) = build_app_and_schema(db);

    let denied = schema
        .execute(Request::new(ADMIN_INSTANCES_QUERY).data(member_auth("u1", "other", true)))
        .await;
    assert_eq!(expect_error_code(&denied), "UNAUTHENTICATED");

    let allowed = schema
        .execute(Request::new(ADMIN_INSTANCES_QUERY).data(superuser_auth("su1")))
        .await;
    let rows = expect_data(&allowed, "adminInstances");
    let found = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == instance.id)
        .unwrap_or_else(|| panic!("deleted instance missing from adminInstances: {rows:?}"));
    assert_eq!(found["deleted"], true);
}
