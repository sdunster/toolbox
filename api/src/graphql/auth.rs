//! GraphQL field guards.
//!
//! [`AuthRequirement`] is matched exhaustively over every [`AuthInfo`] variant, with
//! no `_ =>` catch-all, in every arm below. That's deliberate: adding a third
//! principal later will force every arm here to be revisited rather than silently
//! falling through to whatever the wildcard happened to do.

use async_graphql::Context;
use async_graphql::ErrorExtensions;
use async_graphql::Guard;

use crate::auth::{AuthInfo, Membership};

/// Build a guard-check failure carrying `extensions.code = "UNAUTHENTICATED"`.
/// Guard failures bypass the resolver entirely — there is no `anyhow::Result` for
/// an `ApiError` to ride through — so the code is set here directly rather than via
/// the central classification in `graphql::mod`'s extension, which only fills in a
/// code when one isn't already present.
fn unauthenticated(message: &str) -> async_graphql::Error {
    async_graphql::Error::new(message).extend_with(|_, e| e.set("code", "UNAUTHENTICATED"))
}

/// `pub(crate)`, not private: `query`/`mutations` need the same "is this
/// caller a member of this instance" check for ticket resolvers, whose
/// authorization has to happen inside the resolver body (per-record, after
/// fetching the ticket) rather than in a static `#[graphql(guard)]` — see
/// `graphql::mutations`' `require_ticket_member` doc comment.
pub(crate) fn is_member(memberships: &[Membership], instance_id: &str) -> bool {
    memberships.iter().any(|m| m.instance_id == instance_id)
}

pub(crate) fn is_owner(memberships: &[Membership], instance_id: &str) -> bool {
    memberships
        .iter()
        .any(|m| m.instance_id == instance_id && m.is_owner)
}

pub enum AuthRequirement {
    /// Any authenticated principal: a `User`, or a `Requester` holding a submit
    /// capability token. **Never `ApiToken`** — an integration token is scoped
    /// to exactly one mutation (`submitVerifiedTicket`, guarded by
    /// [`Self::ApiToken`] below), not to "anything a logged-in caller could
    /// do"; widening this to admit it would hand a leaked integration token
    /// reach into `createAttachmentUpload` and everything else this guards.
    Authenticated,
    /// A `User` who belongs (in any role) to this instance.
    Member(String),
    /// A `User` who is an owner of this instance.
    InstanceOwner(String),
    /// The public submit form's capability token — never a `User`, however
    /// privileged.
    Requester,
    /// A `User` with `db::User::superuser` set. **Never satisfied by
    /// `Member`/`InstanceOwner` membership, however privileged** — the
    /// superuser boundary in `CLAUDE.md` is admin + instance settings only,
    /// with no implicit ticket access, so this requirement and
    /// `Member`/`InstanceOwner` are deliberately disjoint rather than one
    /// implying the other.
    Superuser,
    /// A `User` who is either an owner of this instance *or* a superuser —
    /// the combined guard for instance-settings fields/mutations
    /// (`inboundAddresses`, `apiTokens`, `addInboundAddress`,
    /// `removeInboundAddress`) that an owner already manages day to day and
    /// that a superuser must also be able to reach for support/admin
    /// purposes. Every *ticket*-facing guard stays plain `Member`/
    /// `InstanceOwner` — see [`Self::Superuser`]'s doc comment.
    InstanceOwnerOrSuperuser(String),
    /// A `User` who belongs (in any role) to this instance *or* is a
    /// superuser. Guards exactly `Instance.members`: any member needs the
    /// team list to hand a ticket to a colleague, and a superuser's admin UI
    /// manages membership. The member list is not ticket access, so this
    /// does not widen the superuser boundary — a superuser still fails plain
    /// `Member`.
    MemberOrSuperuser(String),
    /// The instance-scoped `mta_` integration token: satisfied only by
    /// `AuthInfo::ApiToken`, never by a `User` (however privileged) or a
    /// `Requester`. Guards exactly `submitVerifiedTicket` — see
    /// `AuthInfo::ApiToken`'s doc comment for what it authorises and why
    /// nothing else does.
    ApiToken,
}

pub struct AuthGuard {
    requirement: AuthRequirement,
}

impl AuthGuard {
    pub fn new(requirement: AuthRequirement) -> Self {
        Self { requirement }
    }
}

impl Guard for AuthGuard {
    async fn check(&self, ctx: &Context<'_>) -> async_graphql::Result<()> {
        let auth = ctx.data_opt::<AuthInfo>();
        match &self.requirement {
            AuthRequirement::Authenticated => {
                if match auth {
                    Some(AuthInfo::User { .. }) => true,
                    Some(AuthInfo::Requester { .. }) => true,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated("Must be authenticated"))
                }
            }
            AuthRequirement::Member(instance_id) => {
                if match auth {
                    Some(AuthInfo::User { memberships, .. }) => is_member(memberships, instance_id),
                    Some(AuthInfo::Requester { .. }) => false,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated("Must be a member of this instance"))
                }
            }
            AuthRequirement::InstanceOwner(instance_id) => {
                if match auth {
                    Some(AuthInfo::User { memberships, .. }) => is_owner(memberships, instance_id),
                    Some(AuthInfo::Requester { .. }) => false,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated("Must be an owner of this instance"))
                }
            }
            AuthRequirement::Requester => {
                if match auth {
                    Some(AuthInfo::Requester { .. }) => true,
                    Some(AuthInfo::User { .. }) => false,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated("Must provide a requester submit token"))
                }
            }
            AuthRequirement::Superuser => {
                if match auth {
                    Some(AuthInfo::User { is_superuser, .. }) => *is_superuser,
                    Some(AuthInfo::Requester { .. }) => false,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated("Must be a superuser"))
                }
            }
            AuthRequirement::InstanceOwnerOrSuperuser(instance_id) => {
                if match auth {
                    Some(AuthInfo::User {
                        memberships,
                        is_superuser,
                        ..
                    }) => *is_superuser || is_owner(memberships, instance_id),
                    Some(AuthInfo::Requester { .. }) => false,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated(
                        "Must be an owner of this instance, or a superuser",
                    ))
                }
            }
            AuthRequirement::MemberOrSuperuser(instance_id) => {
                if match auth {
                    Some(AuthInfo::User {
                        memberships,
                        is_superuser,
                        ..
                    }) => *is_superuser || is_member(memberships, instance_id),
                    Some(AuthInfo::Requester { .. }) => false,
                    Some(AuthInfo::ApiToken { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated(
                        "Must be a member of this instance, or a superuser",
                    ))
                }
            }
            AuthRequirement::ApiToken => {
                if match auth {
                    Some(AuthInfo::ApiToken { .. }) => true,
                    Some(AuthInfo::User { .. }) => false,
                    Some(AuthInfo::Requester { .. }) => false,
                    None => false,
                } {
                    Ok(())
                } else {
                    Err(unauthenticated("Must provide a valid API token"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use crate::mockdb;
    use crate::mockmail;
    use crate::mockstorage;
    use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema};

    /// A tiny ad hoc schema with one field per [`AuthRequirement`] variant, so
    /// the guard truth table below can drive `AuthGuard::check` through
    /// `Schema::execute` (which is what actually builds the `Context` a
    /// `Guard` runs against) instead of calling `check` directly against a
    /// hand-built one.
    struct TruthTableQuery;

    #[Object]
    impl TruthTableQuery {
        #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
        async fn authenticated(&self) -> bool {
            true
        }
        #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(\"inst1\".to_string()))")]
        async fn member(&self) -> bool {
            true
        }
        #[graphql(guard = "AuthGuard::new(AuthRequirement::InstanceOwner(\"inst1\".to_string()))")]
        async fn instance_owner(&self) -> bool {
            true
        }
        #[graphql(guard = "AuthGuard::new(AuthRequirement::Requester)")]
        async fn requester(&self) -> bool {
            true
        }
        #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
        async fn superuser(&self) -> bool {
            true
        }
        #[graphql(
            guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(\"inst1\".to_string()))"
        )]
        async fn instance_owner_or_superuser(&self) -> bool {
            true
        }
        #[graphql(
            guard = "AuthGuard::new(AuthRequirement::MemberOrSuperuser(\"inst1\".to_string()))"
        )]
        async fn member_or_superuser(&self) -> bool {
            true
        }
        #[graphql(guard = "AuthGuard::new(AuthRequirement::ApiToken)")]
        async fn api_token(&self) -> bool {
            true
        }
    }

    fn build_test_schema() -> Schema<TruthTableQuery, EmptyMutation, EmptySubscription> {
        let my_app = std::sync::Arc::new(app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        ));
        Schema::build(TruthTableQuery, EmptyMutation, EmptySubscription)
            .data(my_app)
            .finish()
    }

    fn owner_of_inst1() -> AuthInfo {
        AuthInfo::User {
            id: "u1".into(),
            memberships: vec![Membership {
                instance_id: "inst1".into(),
                is_owner: true,
            }],
            is_superuser: false,
            token_id: None,
            grant_id: None,
        }
    }

    fn agent_of_inst1() -> AuthInfo {
        AuthInfo::User {
            id: "u1".into(),
            memberships: vec![Membership {
                instance_id: "inst1".into(),
                is_owner: false,
            }],
            is_superuser: false,
            token_id: None,
            grant_id: None,
        }
    }

    fn member_of_other_instance() -> AuthInfo {
        AuthInfo::User {
            id: "u2".into(),
            memberships: vec![Membership {
                instance_id: "inst2".into(),
                is_owner: true,
            }],
            is_superuser: false,
            token_id: None,
            grant_id: None,
        }
    }

    fn superuser_no_memberships() -> AuthInfo {
        AuthInfo::User {
            id: "u1".into(),
            memberships: vec![],
            is_superuser: true,
            token_id: None,
            grant_id: None,
        }
    }

    fn requester() -> AuthInfo {
        AuthInfo::Requester {
            email: "r@example.com".into(),
            instance_id: "inst1".into(),
        }
    }

    fn api_token() -> AuthInfo {
        AuthInfo::ApiToken {
            token_id: "tok1".into(),
            instance_id: "inst1".into(),
        }
    }

    async fn field_ok(auth: Option<AuthInfo>, field: &str) -> bool {
        let schema = build_test_schema();
        let query = format!("{{ {field} }}");
        let response = match auth {
            Some(a) => {
                let mut req = async_graphql::Request::new(query);
                req = req.data(a);
                schema.execute(req).await
            }
            None => schema.execute(query).await,
        };
        response.errors.is_empty()
    }

    /// The guard truth table for the two new requirements this feature
    /// introduces. Every row also exercises the pre-existing requirements as
    /// a regression check that nothing above widened or narrowed them.
    #[tokio::test]
    async fn superuser_guard_truth_table() {
        // No credentials at all: every guard rejects.
        assert!(!field_ok(None, "superuser").await);
        assert!(!field_ok(None, "instanceOwnerOrSuperuser").await);

        // A plain member (not owner, not superuser) of inst1: rejected by
        // both.
        assert!(!field_ok(Some(agent_of_inst1()), "superuser").await);
        assert!(!field_ok(Some(agent_of_inst1()), "instanceOwnerOrSuperuser").await);

        // An owner of inst1, but not a superuser: InstanceOwnerOrSuperuser
        // passes (via ownership), plain Superuser still rejects.
        assert!(!field_ok(Some(owner_of_inst1()), "superuser").await);
        assert!(field_ok(Some(owner_of_inst1()), "instanceOwnerOrSuperuser").await);

        // A superuser with no memberships at all: Superuser passes,
        // InstanceOwnerOrSuperuser passes (via superuser), but plain
        // Member/InstanceOwner must still reject — the superuser boundary is
        // never implicit ticket/membership access.
        assert!(field_ok(Some(superuser_no_memberships()), "superuser").await);
        assert!(field_ok(Some(superuser_no_memberships()), "instanceOwnerOrSuperuser").await);
        assert!(!field_ok(Some(superuser_no_memberships()), "member").await);
        assert!(!field_ok(Some(superuser_no_memberships()), "instanceOwner").await);

        // A Requester capability token is never a superuser, however it's
        // used.
        assert!(!field_ok(Some(requester()), "superuser").await);
        assert!(!field_ok(Some(requester()), "instanceOwnerOrSuperuser").await);
        // But Requester still passes its own requirement, unaffected.
        assert!(field_ok(Some(requester()), "requester").await);
    }

    /// An `ApiToken` principal passes only `AuthRequirement::ApiToken` —
    /// every other requirement rejects it, including `Authenticated` (see
    /// that variant's doc comment for why widening it would be a mistake).
    /// A `User`/`Requester` never passes `ApiToken` either, however
    /// privileged.
    #[tokio::test]
    async fn api_token_guard_truth_table() {
        assert!(field_ok(Some(api_token()), "apiToken").await);
        assert!(!field_ok(Some(api_token()), "authenticated").await);
        assert!(!field_ok(Some(api_token()), "member").await);
        assert!(!field_ok(Some(api_token()), "instanceOwner").await);
        assert!(!field_ok(Some(api_token()), "requester").await);
        assert!(!field_ok(Some(api_token()), "superuser").await);
        assert!(!field_ok(Some(api_token()), "instanceOwnerOrSuperuser").await);

        assert!(!field_ok(None, "apiToken").await);
        assert!(!field_ok(Some(owner_of_inst1()), "apiToken").await);
        assert!(!field_ok(Some(superuser_no_memberships()), "apiToken").await);
        assert!(!field_ok(Some(requester()), "apiToken").await);
        assert!(!field_ok(Some(api_token()), "memberOrSuperuser").await);
    }

    /// `MemberOrSuperuser` (the `Instance.members` guard): any member of the
    /// instance, owner or agent, or a superuser — never a member of some
    /// other instance, a requester, or no credentials at all.
    #[tokio::test]
    async fn member_or_superuser_guard_truth_table() {
        assert!(field_ok(Some(agent_of_inst1()), "memberOrSuperuser").await);
        assert!(field_ok(Some(owner_of_inst1()), "memberOrSuperuser").await);
        assert!(field_ok(Some(superuser_no_memberships()), "memberOrSuperuser").await);
        assert!(!field_ok(Some(member_of_other_instance()), "memberOrSuperuser").await);
        assert!(!field_ok(Some(requester()), "memberOrSuperuser").await);
        assert!(!field_ok(None, "memberOrSuperuser").await);
    }
}
