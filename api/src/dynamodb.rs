//! DynamoDB backend.
//!
//! This module holds the *generic* half only: connecting, naming tables, the
//! read-only guard, hydrating raw rows into typed records, and the batching/paging
//! machinery ([`Handler::get_records`], [`query_all`], [`scan_all`],
//! [`Handler::scan_page`]). Domain-specific query/write methods land on
//! [`crate::db::Handler`] and are implemented here as later steps add them.
//!
//! **House rule: omit optional attributes, never write `Null`.** An absent
//! attribute means "not set"; writing an explicit `Null` breaks sparse GSIs (an
//! attribute has to be *absent*, not null, for a row to drop out of a GSI that
//! projects it) and complicates hydration. Deleting or clearing an optional value
//! means removing the attribute (`REMOVE` in an update expression), not setting it
//! to null. See `CLAUDE.md`.

use crate::db::{self, HasID};
use crate::request_metrics::METRICS;
use anyhow::anyhow;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder;
use aws_sdk_dynamodb::operation::scan::builders::ScanFluentBuilder;
use aws_sdk_dynamodb::types::{
    ConsumedCapacity, Delete, KeysAndAttributes, Put, ReturnConsumedCapacity, ReturnValue,
    TransactWriteItem, Update,
};
use aws_sdk_dynamodb::{Client, types::AttributeValue};
use nanoid::nanoid;
use std::collections::HashMap;

const NANOID_ALPHABET: [char; 62] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I',
    'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'a', 'b',
    'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u',
    'v', 'w', 'x', 'y', 'z',
];

/// Turn a failed `UpdateItem` into a [`db::Error`], recognizing the
/// `condition_expression("attribute_exists(id)")` failure every update method
/// below uses to distinguish "this row doesn't exist" from an infrastructure
/// error — otherwise both look identical to the caller.
fn map_update_err(
    e: SdkError<aws_sdk_dynamodb::operation::update_item::UpdateItemError>,
    not_found_msg: String,
) -> db::Error {
    if let SdkError::ServiceError(ref se) = e
        && se.err().is_conditional_check_failed_exception()
    {
        return db::Error::NotFound(not_found_msg);
    }
    db::Error::Infrastructure(sdk_err_msg(e))
}

/// Extract the most useful info from a DynamoDB SdkError.
/// `{}` just prints "service error"; `{:?}` dumps raw HTTP responses.
/// This gives the DynamoDB error code + message for service errors, or the variant
/// name for infrastructure errors (dispatch failure, timeout, etc.).
fn sdk_err_msg<E: ProvideErrorMetadata>(e: SdkError<E>) -> String {
    match (e.code(), e.message()) {
        (Some(code), Some(msg)) => format!("{code}: {msg}"),
        (Some(code), None) => code.to_string(),
        (None, Some(msg)) => msg.to_string(),
        (None, None) => format!("{e}"),
    }
}

/// Generate a new unique ID for DB entities: a 12-char nanoid over a fixed
/// alphanumeric alphabet.
///
/// <https://alex7kom.github.io/nano-nanoid-cc/?alphabet=0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz&size=12&speed=1000&speedUnit=hour>
pub fn new_id() -> String {
    nanoid!(12, &NANOID_ALPHABET)
}

/// A raw DynamoDB row, with typed accessors that turn "wrong type" and
/// "missing/present" into `Result`/`Option` instead of panics.
#[derive(Clone, Debug, PartialEq)]
pub struct Item(HashMap<String, AttributeValue>);

impl Item {
    /// The item's primary key.
    ///
    /// A row with no `id`, or an `id` that is not a string, is corrupt rather than
    /// merely unexpected — but it is still just one bad row, so this reports a
    /// hydration error and leaves the rest of the table readable.
    pub fn id(&self) -> HydrationResult<String> {
        let value = self
            .0
            .get("id")
            .ok_or_else(|| anyhow!("Encountered an item with a missing id field"))?;
        let id = value
            .as_s()
            .map_err(|_| anyhow!("Encountered an item with an ID that is not a string"))?;
        Ok(id.to_string())
    }

    /// True if the attribute is present at all, regardless of type. Used for
    /// presence-marker attributes (like `ticket`'s `instance_visible`) that encode
    /// state by existence.
    pub fn has_field(&self, field: &str) -> bool {
        self.0.contains_key(field)
    }

    pub fn string_field(&self, field: &str) -> anyhow::Result<Option<String>> {
        if let Some(v) = self.0.get(field) {
            match v {
                AttributeValue::S(s) => Ok(Some(s.to_owned())),
                AttributeValue::Null(_) => Ok(None),
                _ => Err(anyhow!("Item had string field of wrong type: {}", field)),
            }
        } else {
            Ok(None)
        }
    }

    pub fn i64_field(&self, field: &str) -> anyhow::Result<Option<i64>> {
        if let Some(v) = self.0.get(field) {
            if let Ok(n) = v.as_n() {
                if let Ok(n) = n.parse::<i64>() {
                    Ok(Some(n))
                } else {
                    Err(anyhow!("Item had unparseable number field: {}", field))
                }
            } else {
                Err(anyhow!("Item had number field of wrong type: {}", field))
            }
        } else {
            Ok(None)
        }
    }

    pub fn bool_field(&self, field: &str) -> anyhow::Result<Option<bool>> {
        if let Some(v) = self.0.get(field) {
            if let Ok(b) = v.as_bool() {
                Ok(Some(*b))
            } else {
                Err(anyhow!("Item had bool field of wrong type: {}", field))
            }
        } else {
            Ok(None)
        }
    }

    /// Get a string set field, erroring if it is of the wrong type. If missing (or
    /// `Null`), returns an empty `Vec` — a String Set can't itself be empty in
    /// DynamoDB, so an empty set is always represented by the attribute's absence.
    pub fn string_set_field(&self, field: &str) -> anyhow::Result<Vec<String>> {
        if let Some(v) = self.0.get(field) {
            if let Ok(ss) = v.as_ss() {
                Ok(ss.to_owned())
            } else if v.as_null().is_ok() {
                Ok(vec![])
            } else {
                Err(anyhow!(
                    "Item had string set/null field of wrong type: {}",
                    field
                ))
            }
        } else {
            Ok(vec![])
        }
    }

    /// `ticket_message.attachments`: a list of `{s3_key, filename,
    /// content_type, size}` maps. Missing is empty — no write path in this
    /// step ever populates this (see [`db::Attachment`]'s doc comment), but
    /// the parser is written correctly now so a future (step 7) write doesn't
    /// need a matching hydration change.
    pub fn attachment_list_field(&self, field: &str) -> anyhow::Result<Vec<db::Attachment>> {
        let Some(v) = self.0.get(field) else {
            return Ok(vec![]);
        };
        let list = v
            .as_l()
            .map_err(|_| anyhow!("Item had attachments field of wrong type: {}", field))?;
        list.iter()
            .map(|entry| {
                let m = entry
                    .as_m()
                    .map_err(|_| anyhow!("attachments entry in {} is not a map", field))?;
                let item = Item(m.clone());
                Ok(db::Attachment {
                    s3_key: item
                        .string_field("s3_key")?
                        .ok_or_else(|| anyhow!("attachment missing s3_key"))?,
                    filename: item
                        .string_field("filename")?
                        .ok_or_else(|| anyhow!("attachment missing filename"))?,
                    content_type: item
                        .string_field("content_type")?
                        .ok_or_else(|| anyhow!("attachment missing content_type"))?,
                    size: item
                        .i64_field("size")?
                        .ok_or_else(|| anyhow!("attachment missing size"))?
                        as u64,
                })
            })
            .collect()
    }
}

impl TryInto<db::User> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::User, Self::Error> {
        Ok(db::User {
            id: self.id()?,
            email: self
                .string_field("email")?
                .ok_or_else(|| anyhow!("User missing email"))?,
            name: self.string_field("name")?.unwrap_or_default(),
            enabled: self.bool_field("enabled")?.unwrap_or(false),
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("User missing created_at"))? as u64,
            access_time: self.i64_field("access_time")?.map(|i| i as u64),
            superuser: self.bool_field("superuser")?.unwrap_or(false),
        })
    }
}

impl TryInto<db::LoginCode> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::LoginCode, Self::Error> {
        Ok(db::LoginCode {
            email: self
                .string_field("email")?
                .ok_or_else(|| anyhow!("LoginCode missing email"))?,
            code_hash: self
                .string_field("code_hash")?
                .ok_or_else(|| anyhow!("LoginCode missing code_hash"))?,
            expires_at: self
                .i64_field("expires_at")?
                .ok_or_else(|| anyhow!("LoginCode missing expires_at"))?
                as u64,
            attempts: self.i64_field("attempts")?.unwrap_or(0) as u64,
            last_sent_at: self
                .i64_field("last_sent_at")?
                .ok_or_else(|| anyhow!("LoginCode missing last_sent_at"))?
                as u64,
        })
    }
}

impl TryInto<db::UserToken> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::UserToken, Self::Error> {
        Ok(db::UserToken {
            id: self.id()?,
            token_hash: self
                .string_field("token_hash")?
                .ok_or_else(|| anyhow!("UserToken missing token_hash"))?,
            user_id: self
                .string_field("user_id")?
                .ok_or_else(|| anyhow!("UserToken missing user_id"))?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("UserToken missing created_at"))?
                as u64,
            expires_at: self
                .i64_field("expires_at")?
                .ok_or_else(|| anyhow!("UserToken missing expires_at"))?
                as u64,
            last_used_at: self.i64_field("last_used_at")?.map(|i| i as u64),
        })
    }
}

impl TryInto<db::OAuthGrant> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::OAuthGrant, Self::Error> {
        Ok(db::OAuthGrant {
            id: self.id()?,
            user_id: self
                .string_field("user_id")?
                .ok_or_else(|| anyhow!("OAuthGrant missing user_id"))?,
            client_id: self
                .string_field("client_id")?
                .ok_or_else(|| anyhow!("OAuthGrant missing client_id"))?,
            client_name: self.string_field("client_name")?.unwrap_or_default(),
            redirect_uri: self
                .string_field("redirect_uri")?
                .ok_or_else(|| anyhow!("OAuthGrant missing redirect_uri"))?,
            resource: self
                .string_field("resource")?
                .ok_or_else(|| anyhow!("OAuthGrant missing resource"))?,
            scope: self
                .string_field("scope")?
                .ok_or_else(|| anyhow!("OAuthGrant missing scope"))?,
            access_token_hash: self
                .string_field("access_token_hash")?
                .ok_or_else(|| anyhow!("OAuthGrant missing access_token_hash"))?,
            access_expires_at: self
                .i64_field("access_expires_at")?
                .ok_or_else(|| anyhow!("OAuthGrant missing access_expires_at"))?
                as u64,
            refresh_token_hash: self
                .string_field("refresh_token_hash")?
                .ok_or_else(|| anyhow!("OAuthGrant missing refresh_token_hash"))?,
            refresh_expires_at: self
                .i64_field("refresh_expires_at")?
                .ok_or_else(|| anyhow!("OAuthGrant missing refresh_expires_at"))?
                as u64,
            expires_at: self
                .i64_field("expires_at")?
                .ok_or_else(|| anyhow!("OAuthGrant missing expires_at"))?
                as u64,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("OAuthGrant missing created_at"))?
                as u64,
            last_used_at: self.i64_field("last_used_at")?.map(|i| i as u64),
        })
    }
}

impl TryInto<db::ApiToken> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::ApiToken, Self::Error> {
        Ok(db::ApiToken {
            id: self.id()?,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("ApiToken missing instance_id"))?,
            name: self
                .string_field("name")?
                .ok_or_else(|| anyhow!("ApiToken missing name"))?,
            token_hash: self
                .string_field("token_hash")?
                .ok_or_else(|| anyhow!("ApiToken missing token_hash"))?,
            enabled: self
                .bool_field("enabled")?
                .ok_or_else(|| anyhow!("ApiToken missing enabled"))?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("ApiToken missing created_at"))?
                as u64,
            created_by_user_id: self
                .string_field("created_by_user_id")?
                .ok_or_else(|| anyhow!("ApiToken missing created_by_user_id"))?,
            last_used_at: self.i64_field("last_used_at")?.map(|i| i as u64),
        })
    }
}

impl TryInto<db::WebauthnCredential> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::WebauthnCredential, Self::Error> {
        Ok(db::WebauthnCredential {
            id: self.id()?,
            user_id: self
                .string_field("user_id")?
                .ok_or_else(|| anyhow!("WebauthnCredential missing user_id"))?,
            name: self
                .string_field("name")?
                .ok_or_else(|| anyhow!("WebauthnCredential missing name"))?,
            passkey_json: self
                .string_field("passkey")?
                .ok_or_else(|| anyhow!("WebauthnCredential missing passkey_json"))?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("WebauthnCredential missing created_at"))?
                as u64,
            last_used_at: self.i64_field("last_used_at")?.map(|i| i as u64),
        })
    }
}

impl TryInto<db::Instance> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::Instance, Self::Error> {
        let kind = match self.string_field("kind")? {
            None => db::InstanceKind::Support,
            Some(s) => db::InstanceKind::parse(&s)
                .ok_or_else(|| anyhow!("Instance has unrecognized kind: {s}"))?,
        };
        Ok(db::Instance {
            id: self.id()?,
            name: self
                .string_field("name")?
                .ok_or_else(|| anyhow!("Instance missing name"))?,
            slug: self
                .string_field("slug")?
                .ok_or_else(|| anyhow!("Instance missing slug"))?,
            kind,
            public_submission_enabled: self
                .bool_field("public_submission_enabled")?
                .unwrap_or(false),
            from_name: self.string_field("from_name")?.unwrap_or_default(),
            signature: self.string_field("signature")?.unwrap_or_default(),
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("Instance missing created_at"))?
                as u64,
            deleted: self.bool_field("deleted")?.unwrap_or(false),
            business_name: self.string_field("business_name")?,
            business_abn: self.string_field("business_abn")?,
            business_address: self.string_field("business_address")?,
            business_phone: self.string_field("business_phone")?,
            business_email: self.string_field("business_email")?,
            payment_details: self.string_field("payment_details")?,
            gst_registered: self.bool_field("gst_registered")?.unwrap_or(false),
            currency: self.string_field("currency")?,
        })
    }
}

impl TryInto<db::InboundAddress> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::InboundAddress, Self::Error> {
        let address = self
            .string_field("address")?
            .ok_or_else(|| anyhow!("InboundAddress missing address"))?;
        let kind_str = self
            .string_field("kind")?
            .ok_or_else(|| anyhow!("InboundAddress missing kind"))?;
        let kind = db::AddressKind::parse(&kind_str)
            .ok_or_else(|| anyhow!("InboundAddress has unrecognized kind: {kind_str}"))?;
        Ok(db::InboundAddress {
            address,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("InboundAddress missing instance_id"))?,
            kind,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("InboundAddress missing created_at"))?
                as u64,
        })
    }
}

impl TryInto<db::Membership> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::Membership, Self::Error> {
        let role_str = self
            .string_field("role")?
            .ok_or_else(|| anyhow!("Membership missing role"))?;
        let role = db::MembershipRole::parse(&role_str)
            .ok_or_else(|| anyhow!("Membership has unrecognized role: {role_str}"))?;
        let defaults = db::NotificationSettings::default();
        Ok(db::Membership {
            id: self.id()?,
            user_id: self
                .string_field("user_id")?
                .ok_or_else(|| anyhow!("Membership missing user_id"))?,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("Membership missing instance_id"))?,
            role,
            // Each attribute is absent unless it was ever explicitly written
            // (`updateNotificationSettings`) — absent means "use the
            // default", per `db::NotificationSettings`'s doc comment. Once
            // written it holds an explicit `true` or `false`; `Bool`
            // hydration errors (not silently defaults) on the wrong type,
            // same as every other bool field here.
            notification_settings: db::NotificationSettings {
                new_ticket: self
                    .bool_field("notify_new_ticket")?
                    .unwrap_or(defaults.new_ticket),
                assigned_to_me: self
                    .bool_field("notify_assigned_to_me")?
                    .unwrap_or(defaults.assigned_to_me),
                assigned_to_me_updated: self
                    .bool_field("notify_assigned_to_me_updated")?
                    .unwrap_or(defaults.assigned_to_me_updated),
                unassigned_updated: self
                    .bool_field("notify_unassigned_updated")?
                    .unwrap_or(defaults.unassigned_updated),
                assigned_to_others_updated: self
                    .bool_field("notify_assigned_to_others_updated")?
                    .unwrap_or(defaults.assigned_to_others_updated),
            },
        })
    }
}

impl TryInto<db::Project> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::Project, Self::Error> {
        Ok(db::Project {
            id: self.id()?,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("Project missing instance_id"))?,
            name: self
                .string_field("name")?
                .ok_or_else(|| anyhow!("Project missing name"))?,
            client_name: self
                .string_field("client_name")?
                .ok_or_else(|| anyhow!("Project missing client_name"))?,
            client_abn: self.string_field("client_abn")?,
            client_address: self.string_field("client_address")?,
            reference: self.string_field("reference")?,
            archived: self.bool_field("archived")?.unwrap_or(false),
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("Project missing created_at"))?
                as u64,
            updated_at: self
                .i64_field("updated_at")?
                .ok_or_else(|| anyhow!("Project missing updated_at"))?
                as u64,
        })
    }
}

impl TryInto<db::BillableItem> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::BillableItem, Self::Error> {
        Ok(db::BillableItem {
            id: self.id()?,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("BillableItem missing instance_id"))?,
            project_id: self
                .string_field("project_id")?
                .ok_or_else(|| anyhow!("BillableItem missing project_id"))?,
            date: self
                .string_field("date")?
                .ok_or_else(|| anyhow!("BillableItem missing date"))?,
            description: self
                .string_field("description")?
                .ok_or_else(|| anyhow!("BillableItem missing description"))?,
            quantity_hundredths: self
                .i64_field("quantity_hundredths")?
                .ok_or_else(|| anyhow!("BillableItem missing quantity_hundredths"))?,
            unit_price_cents: self
                .i64_field("unit_price_cents")?
                .ok_or_else(|| anyhow!("BillableItem missing unit_price_cents"))?,
            invoice_id: self.string_field("invoice_id")?,
            created_by_user_id: self
                .string_field("created_by_user_id")?
                .ok_or_else(|| anyhow!("BillableItem missing created_by_user_id"))?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("BillableItem missing created_at"))?
                as u64,
            updated_at: self
                .i64_field("updated_at")?
                .ok_or_else(|| anyhow!("BillableItem missing updated_at"))?
                as u64,
        })
    }
}

impl TryInto<db::Invoice> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::Invoice, Self::Error> {
        let status_str = self
            .string_field("status")?
            .ok_or_else(|| anyhow!("Invoice missing status"))?;
        let status = db::InvoiceStatus::parse(&status_str)
            .ok_or_else(|| anyhow!("Invoice has unrecognized status: {status_str}"))?;
        Ok(db::Invoice {
            id: self.id()?,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("Invoice missing instance_id"))?,
            project_id: self
                .string_field("project_id")?
                .ok_or_else(|| anyhow!("Invoice missing project_id"))?,
            status,
            version: self
                .i64_field("version")?
                .ok_or_else(|| anyhow!("Invoice missing version"))? as u64,
            item_ids: self.string_set_field("item_ids")?,
            created_by_user_id: self
                .string_field("created_by_user_id")?
                .ok_or_else(|| anyhow!("Invoice missing created_by_user_id"))?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("Invoice missing created_at"))?
                as u64,
            updated_at: self
                .i64_field("updated_at")?
                .ok_or_else(|| anyhow!("Invoice missing updated_at"))?
                as u64,
            number: self.i64_field("number")?.map(|n| n as u32),
            issue_date: self.string_field("issue_date")?,
            snapshot: self.string_field("snapshot")?,
            total_cents: self.i64_field("total_cents")?,
            finalized_at: self.i64_field("finalized_at")?.map(|t| t as u64),
            finalized_by_user_id: self.string_field("finalized_by_user_id")?,
            paid_date: self.string_field("paid_date")?,
            pdf_s3_key: self.string_field("pdf_s3_key")?,
        })
    }
}

impl TryInto<db::EphemeralState> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::EphemeralState, Self::Error> {
        Ok(db::EphemeralState {
            id: self.id()?,
            kind: self
                .string_field("kind")?
                .ok_or_else(|| anyhow!("EphemeralState missing kind"))?,
            payload: self
                .string_field("payload")?
                .ok_or_else(|| anyhow!("EphemeralState missing payload"))?,
            expires_at: self
                .i64_field("expires_at")?
                .ok_or_else(|| anyhow!("EphemeralState missing expires_at"))?
                as u64,
        })
    }
}

impl TryInto<db::Ticket> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::Ticket, Self::Error> {
        let status_str = self
            .string_field("status")?
            .ok_or_else(|| anyhow!("Ticket missing status"))?;
        let status = db::TicketStatus::parse(&status_str)
            .ok_or_else(|| anyhow!("Ticket has unrecognized status: {status_str}"))?;
        Ok(db::Ticket {
            id: self.id()?,
            instance_id: self
                .string_field("instance_id")?
                .ok_or_else(|| anyhow!("Ticket missing instance_id"))?,
            number: self
                .i64_field("number")?
                .ok_or_else(|| anyhow!("Ticket missing number"))? as u64,
            subject: self
                .string_field("subject")?
                .ok_or_else(|| anyhow!("Ticket missing subject"))?,
            status,
            requester_emails: self.string_set_field("requester_emails")?,
            cc_emails: self.string_set_field("cc_emails")?,
            assignee_user_id: self.string_field("assignee_user_id")?,
            reply_token: self
                .string_field("reply_token")?
                .ok_or_else(|| anyhow!("Ticket missing reply_token"))?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("Ticket missing created_at"))?
                as u64,
            updated_at: self
                .i64_field("updated_at")?
                .ok_or_else(|| anyhow!("Ticket missing updated_at"))?
                as u64,
            last_activity_at: self
                .i64_field("last_activity_at")?
                .ok_or_else(|| anyhow!("Ticket missing last_activity_at"))?
                as u64,
            has_attachments: self.bool_field("has_attachments")?.unwrap_or(false),
        })
    }
}

impl TryInto<db::TicketMessage> for Item {
    type Error = HydrationError;
    fn try_into(self) -> Result<db::TicketMessage, Self::Error> {
        let kind_str = self
            .string_field("kind")?
            .ok_or_else(|| anyhow!("TicketMessage missing kind"))?;
        let kind = db::TicketMessageKind::parse(&kind_str)
            .ok_or_else(|| anyhow!("TicketMessage has unrecognized kind: {kind_str}"))?;
        Ok(db::TicketMessage {
            id: self.id()?,
            ticket_id: self
                .string_field("ticket_id")?
                .ok_or_else(|| anyhow!("TicketMessage missing ticket_id"))?,
            kind,
            author_user_id: self.string_field("author_user_id")?,
            from_email: self.string_field("from_email")?,
            to_emails: self.string_set_field("to_emails")?,
            cc_emails: self.string_set_field("cc_emails")?,
            body_text: self.string_field("body_text")?,
            body_html: self.string_field("body_html")?,
            rfc_message_id: self.string_field("rfc_message_id")?,
            in_reply_to: self.string_field("in_reply_to")?,
            references: self.string_field("references")?,
            attachments: self.attachment_list_field("attachments")?,
            raw_s3_key: self.string_field("raw_s3_key")?,
            created_at: self
                .i64_field("created_at")?
                .ok_or_else(|| anyhow!("TicketMessage missing created_at"))?
                as u64,
        })
    }
}

/// A row that could not be turned into its typed record.
///
/// Carries the offending row's `id` where one could be read, because the whole
/// point of the error is to send someone to look at that row. Without it a failed
/// listing says only which table was being read — which, for a table with tens of
/// thousands of rows, is not an actionable report.
#[derive(Debug)]
pub struct HydrationError {
    source: anyhow::Error,
    record_id: Option<String>,
}

impl HydrationError {
    /// Attach the row's `id`, if it is not already known.
    ///
    /// An inner conversion may already have identified a more specific record, so
    /// an existing id is never overwritten.
    fn with_record_id(mut self, record_id: Option<String>) -> Self {
        if self.record_id.is_none() {
            self.record_id = record_id;
        }
        self
    }

    /// The offending row's `id`, where it was readable. `None` means the row's own
    /// `id` attribute is what is broken.
    pub fn record_id(&self) -> Option<&str> {
        self.record_id.as_deref()
    }
}

impl std::fmt::Display for HydrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.record_id {
            Some(id) => write!(f, "record {id}: {}", self.source),
            None => write!(f, "{}", self.source),
        }
    }
}

impl std::error::Error for HydrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Lets `TryInto` impls keep using `?` on `anyhow` errors. The id is filled in
/// afterwards by [`hydrate_item`], which is the only place that still has the raw
/// row.
impl From<anyhow::Error> for HydrationError {
    fn from(source: anyhow::Error) -> Self {
        Self {
            source,
            record_id: None,
        }
    }
}

pub type HydrationResult<T> = Result<T, HydrationError>;

/// Automatically convert hydration failures to `db::Error::Hydration`.
impl From<HydrationError> for db::Error {
    fn from(value: HydrationError) -> Self {
        db::Error::Hydration(value.to_string())
    }
}

// `Clone` is cheap: `aws_sdk_dynamodb::Client` is `Arc`-backed internally
// (cloning it does not open a new connection), and the two remaining fields
// are a `String`/`bool`. Used by integration tests that need one `Handler`
// to build an `App` (which takes it by value) while keeping another handle
// around for direct assertions afterward.
#[derive(Debug, Clone)]
pub struct Handler {
    table_prefix: String,
    client: Client,
    read_only: bool,
}

impl Handler {
    pub fn table_name(&self, name: &str) -> String {
        format!("{}_{}", self.table_prefix, name)
    }

    pub async fn new(table_prefix: &str, read_only: bool) -> Self {
        let region_provider = RegionProviderChain::default_provider().or_else("ap-southeast-2");
        let config = crate::aws_config_loader()
            .region(region_provider)
            .load()
            .await;
        let client = Client::new(&config);
        Self {
            client,
            table_prefix: table_prefix.to_string(),
            read_only,
        }
    }

    /// Guard every write method calls first: this server was started without
    /// `--enable-mutations` (or the Lambda's `READ_ONLY` env var is set), so writes
    /// are refused rather than silently mutating whatever `DB_PREFIX` this points
    /// at.
    pub fn ensure_writable(&self) -> db::Result<()> {
        if self.read_only {
            Err(db::Error::MutationDisabled)
        } else {
            Ok(())
        }
    }

    /// One page of a base-table scan, hydrated leniently so a corrupt row is
    /// reported rather than failing the page. See [`db::ScanPage`] for why an empty
    /// page does not mean the walk is over.
    pub async fn scan_page<R>(
        &self,
        op: &'static str,
        name: &str,
        cursor: Option<db::ScanCursor>,
        limit: i32,
    ) -> db::Result<db::ScanPage<R>>
    where
        Item: TryInto<R, Error = HydrationError>,
    {
        let mut builder = self
            .client
            .scan()
            .table_name(self.table_name(name))
            .limit(limit)
            .return_consumed_capacity(ReturnConsumedCapacity::Total);
        if let Some(cursor) = cursor {
            builder = builder.set_exclusive_start_key(Some(HashMap::from([(
                "id".to_string(),
                AttributeValue::S(cursor.last_id),
            )])));
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(op, resp.consumed_capacity(), CapKind::Read);

        Ok(db::ScanPage {
            rows: hydrate_items_lenient(resp.items)
                .into_iter()
                .map(|row| row.map_err(db::Error::from))
                .collect(),
            next: scan_cursor_from_key(resp.last_evaluated_key),
        })
    }

    /// Batch-fetch rows by id, positionally aligned with `ids` (a `None` names
    /// exactly which requested id is missing).
    ///
    /// Chunks into groups of 100 (DynamoDB's `BatchGetItem` limit) and retries
    /// `UnprocessedKeys` with backoff — DynamoDB is allowed to return fewer items
    /// than requested (under throttling, or when a response would exceed 16MB) and
    /// defers the rest there; skipping the retry would make an existing row look
    /// like it doesn't exist.
    pub async fn get_records<R, T>(&self, name: &str, ids: &[T]) -> db::Result<Vec<Option<R>>>
    where
        T: AsRef<str> + Sync,
        R: HasID + 'static,
        Item: TryInto<R, Error = HydrationError>,
    {
        let ids = ids.iter().map(|id| id.as_ref()).collect::<Vec<&str>>();
        let table_name = self.table_name(name);
        let mut results: HashMap<String, R> = HashMap::new();

        for chunk in ids.chunks(100) {
            let mut pending: Vec<HashMap<String, AttributeValue>> = chunk
                .iter()
                .map(|id| HashMap::from([("id".to_string(), AttributeValue::S(id.to_string()))]))
                .collect();

            for attempt in 1..=BATCH_GET_MAX_ATTEMPTS {
                if attempt > 1 {
                    tokio::time::sleep(batch_get_backoff(attempt - 1)).await;
                }

                let resp = self
                    .client
                    .batch_get_item()
                    .request_items(
                        table_name.clone(),
                        KeysAndAttributes::builder()
                            .set_keys(Some(pending.clone()))
                            .build()
                            .map_err(|e| db::Error::Infrastructure(e.to_string()))?,
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;

                batch_record_capacity(
                    &format!("batch_get {name}"),
                    resp.consumed_capacity(),
                    CapKind::Read,
                );
                if let Some(mut responses) = resp.responses
                    && let Some(items) = responses.remove(&table_name)
                {
                    for item in items {
                        let rec: R = hydrate_item(item)?;
                        results.insert(rec.id().to_string(), rec);
                    }
                }

                pending = unprocessed_keys_for(&table_name, resp.unprocessed_keys);
                if pending.is_empty() {
                    break;
                }
                tracing::warn!(
                    table = %table_name,
                    unprocessed = pending.len(),
                    attempt,
                    "batch_get returned unprocessed keys; retrying"
                );
            }

            if !pending.is_empty() {
                // Returning `None` for these would be indistinguishable from the
                // rows not existing, so fail loudly instead.
                return Err(db::Error::Infrastructure(format!(
                    "batch_get {table_name}: {} key(s) still unprocessed after {BATCH_GET_MAX_ATTEMPTS} attempts",
                    pending.len(),
                )));
            }
        }

        Ok(ids
            .clone()
            .into_iter()
            .map(|id| results.remove(id))
            .collect())
    }

    /// [`Self::get_records`], with `ConsistentRead` set on every
    /// `BatchGetItem` chunk — the "Consistent BatchGetItem variant" invoices
    /// need (see `db::Handler::get_billable_items_consistent`'s doc
    /// comment). Same chunking/retry behaviour, just not eventually
    /// consistent.
    pub async fn get_records_consistent<R, T>(
        &self,
        name: &str,
        ids: &[T],
    ) -> db::Result<Vec<Option<R>>>
    where
        T: AsRef<str> + Sync,
        R: HasID + 'static,
        Item: TryInto<R, Error = HydrationError>,
    {
        let ids = ids.iter().map(|id| id.as_ref()).collect::<Vec<&str>>();
        let table_name = self.table_name(name);
        let mut results: HashMap<String, R> = HashMap::new();

        for chunk in ids.chunks(100) {
            let mut pending: Vec<HashMap<String, AttributeValue>> = chunk
                .iter()
                .map(|id| HashMap::from([("id".to_string(), AttributeValue::S(id.to_string()))]))
                .collect();

            for attempt in 1..=BATCH_GET_MAX_ATTEMPTS {
                if attempt > 1 {
                    tokio::time::sleep(batch_get_backoff(attempt - 1)).await;
                }

                let resp = self
                    .client
                    .batch_get_item()
                    .request_items(
                        table_name.clone(),
                        KeysAndAttributes::builder()
                            .set_keys(Some(pending.clone()))
                            .consistent_read(true)
                            .build()
                            .map_err(|e| db::Error::Infrastructure(e.to_string()))?,
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;

                batch_record_capacity(
                    &format!("batch_get_consistent {name}"),
                    resp.consumed_capacity(),
                    CapKind::Read,
                );
                if let Some(mut responses) = resp.responses
                    && let Some(items) = responses.remove(&table_name)
                {
                    for item in items {
                        let rec: R = hydrate_item(item)?;
                        results.insert(rec.id().to_string(), rec);
                    }
                }

                pending = unprocessed_keys_for(&table_name, resp.unprocessed_keys);
                if pending.is_empty() {
                    break;
                }
                tracing::warn!(
                    table = %table_name,
                    unprocessed = pending.len(),
                    attempt,
                    "consistent batch_get returned unprocessed keys; retrying"
                );
            }

            if !pending.is_empty() {
                return Err(db::Error::Infrastructure(format!(
                    "batch_get_consistent {table_name}: {} key(s) still unprocessed after {BATCH_GET_MAX_ATTEMPTS} attempts",
                    pending.len(),
                )));
            }
        }

        Ok(ids
            .clone()
            .into_iter()
            .map(|id| results.remove(id))
            .collect())
    }

    /// A strongly consistent `GetItem` by id, generic over the hydrated
    /// type — the single-record counterpart of
    /// [`Self::get_records_consistent`], used for invoice mutations' own
    /// authorization/precondition fetch (see `db::Handler::
    /// get_invoice_consistent`'s doc comment). Mirrors `get_api_token`'s
    /// shape exactly.
    pub async fn get_record_consistent<R>(
        &self,
        op: &'static str,
        name: &str,
        id: &str,
    ) -> db::Result<Option<R>>
    where
        Item: TryInto<R, Error = HydrationError>,
    {
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name(name))
            .key("id", AttributeValue::S(id.to_string()))
            .consistent_read(true)
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(op, resp.consumed_capacity(), CapKind::Read);
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    /// Run a `TransactWriteItems` call — Update/Put/Delete items only (no
    /// `ConditionCheck`, so no extra `dynamodb:ConditionCheckItem` IAM
    /// permission is needed; see CLAUDE.md's "Invoicing" house rule and the
    /// doc comment on `infra/iam.tf`'s `dynamodb:TransactWriteItems` grant).
    /// This is the first use of DynamoDB transactions in this codebase, so
    /// every invoice mutation that needs one goes through this one helper.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` when DynamoDB cancelled
    /// the transaction because one or more items' own `condition_expression`
    /// failed — the "someone changed this between read and write" case
    /// every caller maps to a `CONFLICT` — and `Err` for anything else (a
    /// genuine infrastructure failure). A `TransactionCanceledException` can
    /// also be raised by a size/throughput/in-progress-transaction problem;
    /// those are indistinguishable from a condition failure without parsing
    /// `CancellationReasons` per item, which no caller here needs to do
    /// (every condition failure is already the caller's own "reload and
    /// try again" story, and the rarer non-condition cancellations are
    /// safe to surface the same way — the transaction did not commit).
    async fn transact_write(
        &self,
        op: &'static str,
        items: Vec<TransactWriteItem>,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let resp = self
            .client
            .transact_write_items()
            .set_transact_items(Some(items))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                batch_record_capacity(op, r.consumed_capacity(), CapKind::Write);
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se)) if se.err().is_transaction_canceled_exception() => {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }
}

/// Read a scan's continuation key back into a cursor.
///
/// A base-table scan's `LastEvaluatedKey` is just the primary key, and every
/// scannable table is hash-keyed on a string `id`. A key of any other shape means
/// the assumption no longer holds, so treat it as the end of the walk rather than
/// guessing.
fn scan_cursor_from_key(key: Option<HashMap<String, AttributeValue>>) -> Option<db::ScanCursor> {
    key?.get("id")
        .and_then(|v| v.as_s().ok())
        .map(|id| db::ScanCursor {
            last_id: id.to_string(),
        })
}

/// How many times a single `BatchGetItem` chunk is sent before giving up. Four
/// retries after the first attempt, which at the backoff below spans roughly
/// 750ms — long enough to ride out an ordinary throttle, short enough not to stall
/// a request.
const BATCH_GET_MAX_ATTEMPTS: usize = 5;

/// Delay before retry number `retry` (1-based): 50ms doubling to a 1s ceiling.
fn batch_get_backoff(retry: usize) -> std::time::Duration {
    let ms = 50u64
        .saturating_mul(1u64 << retry.min(8).saturating_sub(1))
        .min(1000);
    std::time::Duration::from_millis(ms)
}

/// The keys DynamoDB deferred for our table, if any.
///
/// `UnprocessedKeys` is keyed by table name and is absent — not empty — when
/// everything was processed, so both shapes have to mean "nothing left to do".
fn unprocessed_keys_for(
    table_name: &str,
    unprocessed: Option<HashMap<String, KeysAndAttributes>>,
) -> Vec<HashMap<String, AttributeValue>> {
    unprocessed
        .and_then(|mut tables| tables.remove(table_name))
        .map(|ka| ka.keys)
        .unwrap_or_default()
}

enum CapKind {
    Read,
    Write,
}

fn record_capacity(desc: &str, cap: Option<&ConsumedCapacity>, kind: CapKind) {
    let rcu = cap.and_then(|c| c.read_capacity_units());
    let wcu = cap.and_then(|c| c.write_capacity_units());
    let (rcu, wcu) = if rcu.is_some() || wcu.is_some() {
        (rcu.unwrap_or(0.0), wcu.unwrap_or(0.0))
    } else {
        let total = cap.and_then(|c| c.capacity_units()).unwrap_or(0.0);
        match kind {
            CapKind::Read => (total, 0.0),
            CapKind::Write => (0.0, total),
        }
    };
    let _ = METRICS.try_with(|m| m.record(desc, rcu, wcu));
}

fn batch_record_capacity(desc: &str, caps: &[ConsumedCapacity], kind: CapKind) {
    let rcu_sum: f64 = caps.iter().filter_map(|c| c.read_capacity_units()).sum();
    let wcu_sum: f64 = caps.iter().filter_map(|c| c.write_capacity_units()).sum();
    let (rcu, wcu) = if rcu_sum > 0.0 || wcu_sum > 0.0 {
        (rcu_sum, wcu_sum)
    } else {
        let total: f64 = caps.iter().filter_map(|c| c.capacity_units()).sum();
        match kind {
            CapKind::Read => (total, 0.0),
            CapKind::Write => (0.0, total),
        }
    };
    let _ = METRICS.try_with(|m| m.record(desc, rcu, wcu));
}

/// Hydrate one raw row, tagging any failure with that row's `id`.
///
/// The id is read before the conversion consumes the item, so a row that fails on
/// some *other* attribute can still be named. A row whose `id` is itself unreadable
/// reports no id — the error message then describes the `id` problem directly.
fn hydrate_item<T>(raw: HashMap<String, AttributeValue>) -> HydrationResult<T>
where
    Item: TryInto<T, Error = HydrationError>,
{
    let item = Item(raw);
    let record_id = item.id().ok();
    item.try_into().map_err(|e| e.with_record_id(record_id))
}

/// Hydrate a page of rows, stopping at the first bad one.
///
/// This is the default because production read paths should fail loudly rather
/// than quietly serve a short list. Callers that need to survey every bad row in
/// one pass want [`hydrate_items_lenient`] instead.
fn hydrate_items<T>(items: Option<Vec<HashMap<String, AttributeValue>>>) -> HydrationResult<Vec<T>>
where
    Item: TryInto<T, Error = HydrationError>,
{
    items
        .unwrap_or_default()
        .into_iter()
        .map(hydrate_item)
        .collect()
}

/// Hydrate a page of rows, reporting each row's outcome independently.
///
/// One corrupt row hides every row after it under [`hydrate_items`]. Here the
/// caller sees every failure at once, each already tagged with its record id.
pub fn hydrate_items_lenient<T>(
    items: Option<Vec<HashMap<String, AttributeValue>>>,
) -> Vec<HydrationResult<T>>
where
    Item: TryInto<T, Error = HydrationError>,
{
    items
        .unwrap_or_default()
        .into_iter()
        .map(hydrate_item)
        .collect()
}

/// Run a query to exhaustion, following DynamoDB's 1MB-per-page continuation key.
///
/// `build` is invoked once per page because the SDK's fluent builders are not
/// `Clone`; it must produce an identically-configured request every time.
/// `ExclusiveStartKey` and `ReturnConsumedCapacity` are applied by this helper, and
/// capacity is recorded per page so the metrics reflect the real cost of the full
/// walk.
///
/// The loop breaks only when DynamoDB reports no continuation key. With a
/// `FilterExpression` a page can come back with zero items and still have more to
/// read, so breaking on an empty item list would silently truncate.
pub async fn query_all_items(
    op: &'static str,
    build: impl Fn() -> QueryFluentBuilder,
) -> db::Result<Vec<HashMap<String, AttributeValue>>> {
    let mut items: Vec<HashMap<String, AttributeValue>> = Vec::new();
    let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> = None;

    loop {
        let mut builder = build().return_consumed_capacity(ReturnConsumedCapacity::Total);
        if let Some(esk) = exclusive_start_key.take() {
            builder = builder.set_exclusive_start_key(Some(esk));
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(op, resp.consumed_capacity(), CapKind::Read);

        items.extend(resp.items.unwrap_or_default());
        exclusive_start_key = resp.last_evaluated_key;
        if exclusive_start_key.is_none() {
            return Ok(items);
        }
    }
}

/// [`query_all_items`], hydrated into typed records.
pub async fn query_all<T>(
    op: &'static str,
    build: impl Fn() -> QueryFluentBuilder,
) -> db::Result<Vec<T>>
where
    Item: TryInto<T, Error = HydrationError>,
{
    Ok(hydrate_items(Some(query_all_items(op, build).await?))?)
}

/// Scan equivalent of [`query_all_items`]. Same 1MB paging rules apply.
pub async fn scan_all_items(
    op: &'static str,
    build: impl Fn() -> ScanFluentBuilder,
) -> db::Result<Vec<HashMap<String, AttributeValue>>> {
    let mut items: Vec<HashMap<String, AttributeValue>> = Vec::new();
    let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> = None;

    loop {
        let mut builder = build().return_consumed_capacity(ReturnConsumedCapacity::Total);
        if let Some(esk) = exclusive_start_key.take() {
            builder = builder.set_exclusive_start_key(Some(esk));
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(op, resp.consumed_capacity(), CapKind::Read);

        items.extend(resp.items.unwrap_or_default());
        exclusive_start_key = resp.last_evaluated_key;
        if exclusive_start_key.is_none() {
            return Ok(items);
        }
    }
}

/// [`scan_all_items`], hydrated into typed records.
pub async fn scan_all<T>(
    op: &'static str,
    build: impl Fn() -> ScanFluentBuilder,
) -> db::Result<Vec<T>>
where
    Item: TryInto<T, Error = HydrationError>,
{
    Ok(hydrate_items(Some(scan_all_items(op, build).await?))?)
}

/// Which direction to scan a keyset-paginated GSI query, and whether the
/// fetched page needs reversing before it is returned. Ported from seslogin's
/// `dynamodb.rs` (same name, same signature) — see that file's periods
/// listing for the reference implementation this mirrors.
///
/// `descending` is the list's natural order (newest-activity-first for
/// tickets). `has_after`/`has_before` name which cursor the caller supplied
/// (at most one — `pagination_args` rejects both `first`/`after` combined
/// with `last`/`before` before this is ever called with both true).
fn page_scan_direction(has_after: bool, has_before: bool, descending: bool) -> (bool, bool) {
    let scan_forward = match (has_after, has_before) {
        (true, _) => !descending,
        (false, true) => descending,
        (false, false) => !descending,
    };
    (scan_forward, has_before && !has_after)
}

impl db::Handler for Handler {
    // ── instance ──────────────────────────────────────────────────────────

    async fn get_instances<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::Instance>>> {
        self.get_records("instance", ids).await
    }

    async fn get_instance_id_by_slug(&self, slug: &str) -> db::Result<Option<String>> {
        let resp = self
            .client
            .query()
            .table_name(self.table_name("instance"))
            .index_name("slug-index")
            .key_condition_expression("slug = :slug")
            .expression_attribute_values(":slug", AttributeValue::S(slug.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_instance_id_by_slug",
            resp.consumed_capacity(),
            CapKind::Read,
        );

        let ids = resp
            .items
            .unwrap_or_default()
            .into_iter()
            .map(|item| Item(item).id())
            .collect::<HydrationResult<Vec<String>>>()?;
        db::at_most_one(ids, || format!("Multiple instances share slug {slug}"))
    }

    async fn create_instance(
        &self,
        name: &str,
        slug: &str,
        from_name: &str,
        signature: &str,
        public_submission_enabled: bool,
        kind: db::InstanceKind,
    ) -> db::Result<db::Instance> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();

        let mut req = self
            .client
            .put_item()
            .table_name(self.table_name("instance"))
            .item("id", AttributeValue::S(id.clone()))
            .item("name", AttributeValue::S(name.to_string()))
            .item("slug", AttributeValue::S(slug.to_string()))
            .item("from_name", AttributeValue::S(from_name.to_string()))
            .item("signature", AttributeValue::S(signature.to_string()))
            .item("created_at", AttributeValue::N(now.to_string()))
            // slug is not itself the primary key, so this cannot enforce
            // uniqueness — it only guards against generating a colliding
            // nanoid, which is astronomically unlikely. See the doc comment on
            // `db::Handler::create_instance`.
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total);
        // Omit-optional-attributes house rule: only written when true/non-Support.
        if public_submission_enabled {
            req = req.item("public_submission_enabled", AttributeValue::Bool(true));
        }
        if kind == db::InstanceKind::Invoicing {
            req = req.item("kind", AttributeValue::S(kind.as_str().to_string()));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("create_instance", resp.consumed_capacity(), CapKind::Write);

        Ok(db::Instance {
            id,
            name: name.to_string(),
            slug: slug.to_string(),
            kind,
            public_submission_enabled,
            from_name: from_name.to_string(),
            signature: signature.to_string(),
            created_at: now,
            deleted: false,
            business_name: None,
            business_abn: None,
            business_address: None,
            business_phone: None,
            business_email: None,
            payment_details: None,
            gst_registered: false,
            currency: None,
        })
    }

    async fn update_instance(
        &self,
        id: &str,
        change: db::InstanceUpdateShape<'_>,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::InstanceUpdateShape::Fields {
                name,
                from_name,
                signature,
                public_submission_enabled,
            } => {
                let mut update_expr =
                    "SET #n = :name, from_name = :from_name, signature = :signature".to_string();
                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("instance"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .expression_attribute_names("#n", "name")
                    .expression_attribute_values(":name", AttributeValue::S(name.to_string()))
                    .expression_attribute_values(
                        ":from_name",
                        AttributeValue::S(from_name.to_string()),
                    )
                    .expression_attribute_values(
                        ":signature",
                        AttributeValue::S(signature.to_string()),
                    );
                if public_submission_enabled {
                    update_expr.push_str(", public_submission_enabled = :pse");
                    req = req.expression_attribute_values(":pse", AttributeValue::Bool(true));
                } else {
                    update_expr.push_str(" REMOVE public_submission_enabled");
                }
                let resp = req
                    .update_expression(update_expr)
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Instance {id}")))?;
                record_capacity("update_instance", resp.consumed_capacity(), CapKind::Write);
            }
            db::InstanceUpdateShape::SetDeleted(deleted) => {
                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("instance"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)");
                if deleted {
                    req = req
                        .update_expression("SET deleted = :deleted")
                        .expression_attribute_values(":deleted", AttributeValue::Bool(true));
                } else {
                    req = req.update_expression("REMOVE deleted");
                }
                let resp = req
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Instance {id}")))?;
                record_capacity(
                    "update_instance_deleted",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::InstanceUpdateShape::SetInvoicingSettings {
                business_name,
                business_abn,
                business_address,
                business_phone,
                business_email,
                payment_details,
                gst_registered,
                currency,
            } => {
                let mut sets: Vec<String> = Vec::new();
                let mut removes: Vec<&str> = Vec::new();
                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("instance"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)");

                let string_fields: [(&str, &str, Option<&str>); 6] = [
                    (":bn", "business_name", business_name),
                    (":ba", "business_abn", business_abn),
                    (":bad", "business_address", business_address),
                    (":bp", "business_phone", business_phone),
                    (":be", "business_email", business_email),
                    (":pd", "payment_details", payment_details),
                    // currency handled separately below (own placeholder).
                ];
                for (placeholder, attr, value) in string_fields {
                    match value {
                        Some(v) => {
                            sets.push(format!("{attr} = {placeholder}"));
                            req = req.expression_attribute_values(
                                placeholder,
                                AttributeValue::S(v.to_string()),
                            );
                        }
                        None => removes.push(attr),
                    }
                }
                match currency {
                    Some(c) => {
                        sets.push("currency = :cur".to_string());
                        req = req
                            .expression_attribute_values(":cur", AttributeValue::S(c.to_string()));
                    }
                    None => removes.push("currency"),
                }
                // Omit-optional-attributes house rule: only ever written `true`.
                if gst_registered {
                    sets.push("gst_registered = :gst".to_string());
                    req = req.expression_attribute_values(":gst", AttributeValue::Bool(true));
                } else {
                    removes.push("gst_registered");
                }

                let mut update_expr = String::new();
                if !sets.is_empty() {
                    update_expr.push_str("SET ");
                    update_expr.push_str(&sets.join(", "));
                }
                if !removes.is_empty() {
                    if !update_expr.is_empty() {
                        update_expr.push(' ');
                    }
                    update_expr.push_str("REMOVE ");
                    update_expr.push_str(&removes.join(", "));
                }
                let resp = req
                    .update_expression(update_expr)
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Instance {id}")))?;
                record_capacity(
                    "update_instance_invoicing_settings",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn list_instances(&self) -> db::Result<Vec<db::Instance>> {
        scan_all("list_instances", || {
            self.client.scan().table_name(self.table_name("instance"))
        })
        .await
    }

    // ── inbound_address ──────────────────────────────────────────────────────

    async fn create_inbound_address(
        &self,
        address: &str,
        instance_id: &str,
        kind: db::AddressKind,
    ) -> db::Result<db::InboundAddress> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("inbound_address"))
            .item("address", AttributeValue::S(address.to_string()))
            .item("instance_id", AttributeValue::S(instance_id.to_string()))
            .item("kind", AttributeValue::S(kind.as_str().to_string()))
            .item("created_at", AttributeValue::N(now.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_inbound_address",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(db::InboundAddress {
            address: address.to_string(),
            instance_id: instance_id.to_string(),
            kind,
            created_at: now,
        })
    }

    async fn get_inbound_address(&self, address: &str) -> db::Result<Option<db::InboundAddress>> {
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("inbound_address"))
            .key("address", AttributeValue::S(address.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_inbound_address",
            resp.consumed_capacity(),
            CapKind::Read,
        );
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn delete_inbound_address(&self, address: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("inbound_address"))
            .key("address", AttributeValue::S(address.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    async fn list_inbound_addresses_by_instance(
        &self,
        instance_id: &str,
    ) -> db::Result<Vec<db::InboundAddress>> {
        query_all("list_inbound_addresses_by_instance", || {
            self.client
                .query()
                .table_name(self.table_name("inbound_address"))
                .index_name("instance_id-index")
                .key_condition_expression("instance_id = :instance_id")
                .expression_attribute_values(
                    ":instance_id",
                    AttributeValue::S(instance_id.to_string()),
                )
        })
        .await
    }

    // ── membership ────────────────────────────────────────────────────────

    async fn create_membership(
        &self,
        user_id: &str,
        instance_id: &str,
        role: db::MembershipRole,
    ) -> db::Result<db::Membership> {
        self.ensure_writable()?;
        let id = new_id();
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("membership"))
            .item("id", AttributeValue::S(id.clone()))
            .item("user_id", AttributeValue::S(user_id.to_string()))
            .item("instance_id", AttributeValue::S(instance_id.to_string()))
            .item("role", AttributeValue::S(role.as_str().to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_membership",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(db::Membership {
            id,
            user_id: user_id.to_string(),
            instance_id: instance_id.to_string(),
            role,
            // No `notify_*` attributes are written here — per the
            // omit-optional-attributes house rule, a brand-new membership
            // (and one re-added after removal) starts at
            // `NotificationSettings::default()` by simple absence.
            notification_settings: db::NotificationSettings::default(),
        })
    }

    async fn delete_membership(&self, id: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("membership"))
            .key("id", AttributeValue::S(id.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    async fn update_membership_role(&self, id: &str, role: db::MembershipRole) -> db::Result<()> {
        self.ensure_writable()?;
        let resp = self
            .client
            .update_item()
            .table_name(self.table_name("membership"))
            .key("id", AttributeValue::S(id.to_string()))
            .condition_expression("attribute_exists(id)")
            .update_expression("SET #r = :role")
            .expression_attribute_names("#r", "role")
            .expression_attribute_values(":role", AttributeValue::S(role.as_str().to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| map_update_err(e, format!("Membership {id}")))?;
        record_capacity(
            "update_membership_role",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(())
    }

    async fn update_membership_notification_settings(
        &self,
        id: &str,
        patch: &db::NotificationSettingsPatch,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        if patch.is_empty() {
            // No fields set — nothing to write. An `UpdateItem` with an
            // empty `SET` clause is a request error, not a no-op, so this
            // has to be caught here rather than left to DynamoDB.
            return Ok(());
        }

        let fields: [(&str, &str, Option<bool>); 5] = [
            (":nt", "notify_new_ticket", patch.new_ticket),
            (":am", "notify_assigned_to_me", patch.assigned_to_me),
            (
                ":amu",
                "notify_assigned_to_me_updated",
                patch.assigned_to_me_updated,
            ),
            (":uu", "notify_unassigned_updated", patch.unassigned_updated),
            (
                ":aou",
                "notify_assigned_to_others_updated",
                patch.assigned_to_others_updated,
            ),
        ];

        let mut sets = Vec::new();
        let mut req = self
            .client
            .update_item()
            .table_name(self.table_name("membership"))
            .key("id", AttributeValue::S(id.to_string()))
            .condition_expression("attribute_exists(id)");
        for (placeholder, attr, value) in fields {
            if let Some(v) = value {
                sets.push(format!("{attr} = {placeholder}"));
                req = req.expression_attribute_values(placeholder, AttributeValue::Bool(v));
            }
        }

        let resp = req
            .update_expression(format!("SET {}", sets.join(", ")))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| map_update_err(e, format!("Membership {id}")))?;
        record_capacity(
            "update_membership_notification_settings",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(())
    }

    async fn list_memberships_by_user(&self, user_id: &str) -> db::Result<Vec<db::Membership>> {
        query_all("list_memberships_by_user", || {
            self.client
                .query()
                .table_name(self.table_name("membership"))
                .index_name("user_id-index")
                .key_condition_expression("user_id = :user_id")
                .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_string()))
        })
        .await
    }

    async fn list_memberships_by_instance(
        &self,
        instance_id: &str,
    ) -> db::Result<Vec<db::Membership>> {
        query_all("list_memberships_by_instance", || {
            self.client
                .query()
                .table_name(self.table_name("membership"))
                .index_name("instance_id-index")
                .key_condition_expression("instance_id = :instance_id")
                .expression_attribute_values(
                    ":instance_id",
                    AttributeValue::S(instance_id.to_string()),
                )
        })
        .await
    }

    // ── user ──────────────────────────────────────────────────────────────

    async fn get_users<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::User>>> {
        self.get_records("user", ids).await
    }

    async fn get_user_id_by_email(&self, email: &str) -> db::Result<Option<String>> {
        let resp = self
            .client
            .query()
            .table_name(self.table_name("user"))
            .index_name("email-index")
            .key_condition_expression("email = :email")
            .expression_attribute_values(":email", AttributeValue::S(email.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_user_id_by_email",
            resp.consumed_capacity(),
            CapKind::Read,
        );

        let ids = resp
            .items
            .unwrap_or_default()
            .into_iter()
            .map(|item| Item(item).id())
            .collect::<HydrationResult<Vec<String>>>()?;
        db::at_most_one(ids, || format!("Multiple users share email {email}"))
    }

    async fn create_user(&self, email: &str, name: &str) -> db::Result<db::User> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();

        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("user"))
            .item("id", AttributeValue::S(id.clone()))
            .item("email", AttributeValue::S(email.to_string()))
            .item("name", AttributeValue::S(name.to_string()))
            .item("enabled", AttributeValue::Bool(true))
            .item("created_at", AttributeValue::N(now.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("create_user", resp.consumed_capacity(), CapKind::Write);

        Ok(db::User {
            id,
            email: email.to_string(),
            name: name.to_string(),
            enabled: true,
            created_at: now,
            access_time: None,
            superuser: false,
        })
    }

    async fn list_users(&self) -> db::Result<Vec<db::User>> {
        scan_all("list_users", || {
            self.client.scan().table_name(self.table_name("user"))
        })
        .await
    }

    async fn update_user(&self, id: &str, change: db::UserUpdateShape<'_>) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::UserUpdateShape::Fields { name, enabled } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("user"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET #n = :name, enabled = :enabled")
                    .expression_attribute_names("#n", "name")
                    .expression_attribute_values(":name", AttributeValue::S(name.to_string()))
                    .expression_attribute_values(":enabled", AttributeValue::Bool(enabled))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("User {id}")))?;
                record_capacity("update_user", resp.consumed_capacity(), CapKind::Write);
            }
            db::UserUpdateShape::AccessTime => {
                let now = crate::clock::now_sec();
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("user"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET access_time = :access_time")
                    .expression_attribute_values(":access_time", AttributeValue::N(now.to_string()))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("User {id}")))?;
                record_capacity(
                    "update_user_access_time",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::UserUpdateShape::SetSuperuser(superuser) => {
                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("user"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)");
                if superuser {
                    req = req
                        .update_expression("SET superuser = :superuser")
                        .expression_attribute_values(":superuser", AttributeValue::Bool(true));
                } else {
                    req = req.update_expression("REMOVE superuser");
                }
                let resp = req
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("User {id}")))?;
                record_capacity(
                    "update_user_superuser",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::UserUpdateShape::SetEmail { email } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("user"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET email = :email")
                    .expression_attribute_values(":email", AttributeValue::S(email.to_string()))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("User {id}")))?;
                record_capacity(
                    "update_user_email",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    // ── login_code ────────────────────────────────────────────────────────

    async fn put_login_code(
        &self,
        email: &str,
        code_hash: &str,
        expires_at: u64,
        now: u64,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("login_code"))
            .item("email", AttributeValue::S(email.to_string()))
            .item("code_hash", AttributeValue::S(code_hash.to_string()))
            .item("expires_at", AttributeValue::N(expires_at.to_string()))
            .item("attempts", AttributeValue::N("0".to_string()))
            .item("last_sent_at", AttributeValue::N(now.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("put_login_code", resp.consumed_capacity(), CapKind::Write);
        Ok(())
    }

    async fn get_login_code(&self, email: &str) -> db::Result<Option<db::LoginCode>> {
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("login_code"))
            .key("email", AttributeValue::S(email.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("get_login_code", resp.consumed_capacity(), CapKind::Read);
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn delete_login_code(&self, email: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("login_code"))
            .key("email", AttributeValue::S(email.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    async fn increment_login_code_attempts(&self, email: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .update_item()
            .table_name(self.table_name("login_code"))
            .key("email", AttributeValue::S(email.to_string()))
            .update_expression("ADD attempts :one")
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    // ── user_token ────────────────────────────────────────────────────────

    async fn create_user_token(
        &self,
        id: &str,
        token_hash: &str,
        user_id: &str,
        expires_at: u64,
    ) -> db::Result<db::UserToken> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("user_token"))
            .item("id", AttributeValue::S(id.to_string()))
            .item("token_hash", AttributeValue::S(token_hash.to_string()))
            .item("user_id", AttributeValue::S(user_id.to_string()))
            .item("created_at", AttributeValue::N(now.to_string()))
            .item("expires_at", AttributeValue::N(expires_at.to_string()))
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_user_token",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(db::UserToken {
            id: id.to_string(),
            token_hash: token_hash.to_string(),
            user_id: user_id.to_string(),
            created_at: now,
            expires_at,
            last_used_at: None,
        })
    }

    async fn get_user_token(&self, id: &str) -> db::Result<Option<db::UserToken>> {
        // Strongly consistent GetItem by the id embedded in the token, same
        // reasoning as get_api_token: a just-issued session must authenticate
        // on its very first request.
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("user_token"))
            .key("id", AttributeValue::S(id.to_string()))
            .consistent_read(true)
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("get_user_token", resp.consumed_capacity(), CapKind::Read);
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn update_user_token(
        &self,
        id: &str,
        change: db::UserTokenUpdateShape,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::UserTokenUpdateShape::TouchLastUsed => {
                let now = crate::clock::now_sec();
                let new_expires_at = crate::expire::ExpirePolicy::UserToken.expires_at(now);
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("user_token"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET last_used_at = :last_used_at, expires_at = :expires_at")
                    .expression_attribute_values(
                        ":last_used_at",
                        AttributeValue::N(now.to_string()),
                    )
                    .expression_attribute_values(
                        ":expires_at",
                        AttributeValue::N(new_expires_at.to_string()),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("UserToken {id}")))?;
                record_capacity(
                    "update_user_token",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn delete_user_token(&self, id: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("user_token"))
            .key("id", AttributeValue::S(id.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    // ── oauth_grant ───────────────────────────────────────────────────────

    async fn create_oauth_grant(&self, grant: &db::OAuthGrant) -> db::Result<()> {
        self.ensure_writable()?;
        let mut req = self
            .client
            .put_item()
            .table_name(self.table_name("oauth_grant"))
            .item("id", AttributeValue::S(grant.id.clone()))
            .item("user_id", AttributeValue::S(grant.user_id.clone()))
            .item("client_id", AttributeValue::S(grant.client_id.clone()))
            .item("client_name", AttributeValue::S(grant.client_name.clone()))
            .item(
                "redirect_uri",
                AttributeValue::S(grant.redirect_uri.clone()),
            )
            .item("resource", AttributeValue::S(grant.resource.clone()))
            .item("scope", AttributeValue::S(grant.scope.clone()))
            .item(
                "access_token_hash",
                AttributeValue::S(grant.access_token_hash.clone()),
            )
            .item(
                "access_expires_at",
                AttributeValue::N(grant.access_expires_at.to_string()),
            )
            .item(
                "refresh_token_hash",
                AttributeValue::S(grant.refresh_token_hash.clone()),
            )
            .item(
                "refresh_expires_at",
                AttributeValue::N(grant.refresh_expires_at.to_string()),
            )
            .item(
                "expires_at",
                AttributeValue::N(grant.expires_at.to_string()),
            )
            .item(
                "created_at",
                AttributeValue::N(grant.created_at.to_string()),
            )
            .condition_expression("attribute_not_exists(id)");
        // Optional attribute: omit rather than write Null (see CLAUDE.md).
        if let Some(last_used_at) = grant.last_used_at {
            req = req.item("last_used_at", AttributeValue::N(last_used_at.to_string()));
        }
        let resp = req
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_oauth_grant",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(())
    }

    async fn get_oauth_grant(&self, id: &str) -> db::Result<Option<db::OAuthGrant>> {
        // Strongly consistent, same reasoning as get_api_token.
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("oauth_grant"))
            .key("id", AttributeValue::S(id.to_string()))
            .consistent_read(true)
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("get_oauth_grant", resp.consumed_capacity(), CapKind::Read);
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn update_oauth_grant(
        &self,
        id: &str,
        change: db::OAuthGrantUpdateShape,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::OAuthGrantUpdateShape::Rotate {
                expected_refresh_token_hash,
                access_token_hash,
                access_expires_at,
                refresh_token_hash,
                refresh_expires_at,
            } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("oauth_grant"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression(
                        "attribute_exists(id) AND refresh_token_hash = :expected_refresh_token_hash",
                    )
                    .update_expression(
                        "SET access_token_hash = :access_token_hash, \
                         access_expires_at = :access_expires_at, \
                         refresh_token_hash = :refresh_token_hash, \
                         refresh_expires_at = :refresh_expires_at",
                    )
                    .expression_attribute_values(
                        ":expected_refresh_token_hash",
                        AttributeValue::S(expected_refresh_token_hash),
                    )
                    .expression_attribute_values(
                        ":access_token_hash",
                        AttributeValue::S(access_token_hash),
                    )
                    .expression_attribute_values(
                        ":access_expires_at",
                        AttributeValue::N(access_expires_at.to_string()),
                    )
                    .expression_attribute_values(
                        ":refresh_token_hash",
                        AttributeValue::S(refresh_token_hash),
                    )
                    .expression_attribute_values(
                        ":refresh_expires_at",
                        AttributeValue::N(refresh_expires_at.to_string()),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("OAuthGrant {id}")))?;
                record_capacity(
                    "update_oauth_grant",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::OAuthGrantUpdateShape::TouchLastUsed => {
                let now = crate::clock::now_sec();
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("oauth_grant"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET last_used_at = :last_used_at")
                    .expression_attribute_values(
                        ":last_used_at",
                        AttributeValue::N(now.to_string()),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("OAuthGrant {id}")))?;
                record_capacity(
                    "update_oauth_grant",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn delete_oauth_grant(&self, id: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("oauth_grant"))
            .key("id", AttributeValue::S(id.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    async fn list_oauth_grants_by_user(&self, user_id: &str) -> db::Result<Vec<db::OAuthGrant>> {
        query_all("list_oauth_grants_by_user", || {
            self.client
                .query()
                .table_name(self.table_name("oauth_grant"))
                .index_name("user_id-index")
                .key_condition_expression("user_id = :user_id")
                .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_string()))
        })
        .await
    }

    // ── api_token ─────────────────────────────────────────────────────────

    async fn create_api_token(
        &self,
        id: &str,
        instance_id: &str,
        name: &str,
        token_hash: &str,
        created_by_user_id: &str,
    ) -> db::Result<db::ApiToken> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("api_token"))
            .item("id", AttributeValue::S(id.to_string()))
            .item("instance_id", AttributeValue::S(instance_id.to_string()))
            .item("name", AttributeValue::S(name.to_string()))
            .item("token_hash", AttributeValue::S(token_hash.to_string()))
            .item("enabled", AttributeValue::Bool(true))
            .item("created_at", AttributeValue::N(now.to_string()))
            .item(
                "created_by_user_id",
                AttributeValue::S(created_by_user_id.to_string()),
            )
            // Same astronomically-unlikely-collision guard as
            // create_instance's identically-shaped condition — `id` here is
            // caller-supplied (see db::Handler::create_api_token's doc
            // comment), not generated by this method, but the invariant it
            // protects is the same.
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("create_api_token", resp.consumed_capacity(), CapKind::Write);
        Ok(db::ApiToken {
            id: id.to_string(),
            instance_id: instance_id.to_string(),
            name: name.to_string(),
            token_hash: token_hash.to_string(),
            enabled: true,
            created_at: now,
            created_by_user_id: created_by_user_id.to_string(),
            last_used_at: None,
        })
    }

    async fn get_api_token(&self, id: &str) -> db::Result<Option<db::ApiToken>> {
        // Strongly consistent GetItem by id — see db::ApiToken's doc comment
        // for why this table has no token_hash GSI to look up by instead.
        // Not `get_records`: its BatchGetItem is eventually consistent, which
        // would reopen exactly the window the id-in-token design closes — a
        // just-minted token failing its first use, or a just-disabled one
        // still being accepted.
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("api_token"))
            .key("id", AttributeValue::S(id.to_string()))
            .consistent_read(true)
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("get_api_token", resp.consumed_capacity(), CapKind::Read);
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn list_api_tokens_by_instance(
        &self,
        instance_id: &str,
    ) -> db::Result<Vec<db::ApiToken>> {
        query_all("list_api_tokens_by_instance", || {
            self.client
                .query()
                .table_name(self.table_name("api_token"))
                .index_name("instance_id-index")
                .key_condition_expression("instance_id = :instance_id")
                .expression_attribute_values(
                    ":instance_id",
                    AttributeValue::S(instance_id.to_string()),
                )
        })
        .await
    }

    async fn update_api_token(
        &self,
        id: &str,
        change: db::ApiTokenUpdateShape<'_>,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::ApiTokenUpdateShape::Fields { name, enabled } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("api_token"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET #n = :name, enabled = :enabled")
                    .expression_attribute_names("#n", "name")
                    .expression_attribute_values(":name", AttributeValue::S(name.to_string()))
                    .expression_attribute_values(":enabled", AttributeValue::Bool(enabled))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("ApiToken {id}")))?;
                record_capacity(
                    "update_api_token_fields",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::ApiTokenUpdateShape::TouchLastUsed => {
                let now = crate::clock::now_sec();
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("api_token"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET last_used_at = :last_used_at")
                    .expression_attribute_values(
                        ":last_used_at",
                        AttributeValue::N(now.to_string()),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("ApiToken {id}")))?;
                record_capacity(
                    "update_api_token_touch_last_used",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn delete_api_token(&self, id: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("api_token"))
            .key("id", AttributeValue::S(id.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    // ── project ───────────────────────────────────────────────────────────

    async fn create_project(
        &self,
        instance_id: &str,
        name: &str,
        client_name: &str,
        client_abn: Option<&str>,
        client_address: Option<&str>,
        reference: Option<&str>,
    ) -> db::Result<db::Project> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();
        let mut req = self
            .client
            .put_item()
            .table_name(self.table_name("project"))
            .item("id", AttributeValue::S(id.clone()))
            .item("instance_id", AttributeValue::S(instance_id.to_string()))
            .item("name", AttributeValue::S(name.to_string()))
            .item("client_name", AttributeValue::S(client_name.to_string()))
            .item("created_at", AttributeValue::N(now.to_string()))
            .item("updated_at", AttributeValue::N(now.to_string()))
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total);
        if let Some(v) = client_abn {
            req = req.item("client_abn", AttributeValue::S(v.to_string()));
        }
        if let Some(v) = client_address {
            req = req.item("client_address", AttributeValue::S(v.to_string()));
        }
        if let Some(v) = reference {
            req = req.item("reference", AttributeValue::S(v.to_string()));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("create_project", resp.consumed_capacity(), CapKind::Write);
        Ok(db::Project {
            id,
            instance_id: instance_id.to_string(),
            name: name.to_string(),
            client_name: client_name.to_string(),
            client_abn: client_abn.map(str::to_string),
            client_address: client_address.map(str::to_string),
            reference: reference.map(str::to_string),
            archived: false,
            created_at: now,
            updated_at: now,
        })
    }

    async fn get_projects<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::Project>>> {
        self.get_records("project", ids).await
    }

    async fn list_projects_by_instance(&self, instance_id: &str) -> db::Result<Vec<db::Project>> {
        query_all("list_projects_by_instance", || {
            self.client
                .query()
                .table_name(self.table_name("project"))
                .index_name("instance_id-index")
                .key_condition_expression("instance_id = :instance_id")
                .expression_attribute_values(
                    ":instance_id",
                    AttributeValue::S(instance_id.to_string()),
                )
        })
        .await
    }

    async fn update_project(&self, id: &str, change: db::ProjectUpdateShape<'_>) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::ProjectUpdateShape::Fields {
                name,
                client_name,
                client_abn,
                client_address,
                reference,
                archived,
            } => {
                let now = crate::clock::now_sec();
                let mut sets = vec![
                    "#n = :name".to_string(),
                    "client_name = :client_name".to_string(),
                    "updated_at = :updated_at".to_string(),
                ];
                let mut removes: Vec<&str> = Vec::new();
                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("project"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .expression_attribute_names("#n", "name")
                    // "reference" is a DynamoDB reserved keyword and cannot
                    // appear literally in an update/condition/projection
                    // expression — aliased via #ref like #n above.
                    .expression_attribute_names("#ref", "reference")
                    .expression_attribute_values(":name", AttributeValue::S(name.to_string()))
                    .expression_attribute_values(
                        ":client_name",
                        AttributeValue::S(client_name.to_string()),
                    )
                    .expression_attribute_values(":updated_at", AttributeValue::N(now.to_string()));

                let optional_fields: [(&str, &str, Option<&str>); 3] = [
                    (":client_abn", "client_abn", client_abn),
                    (":client_address", "client_address", client_address),
                    (":reference", "#ref", reference),
                ];
                for (placeholder, attr, value) in optional_fields {
                    match value {
                        Some(v) => {
                            sets.push(format!("{attr} = {placeholder}"));
                            req = req.expression_attribute_values(
                                placeholder,
                                AttributeValue::S(v.to_string()),
                            );
                        }
                        None => removes.push(attr),
                    }
                }
                // Omit-optional-attributes house rule: only ever written `true`.
                if archived {
                    sets.push("archived = :archived".to_string());
                    req = req.expression_attribute_values(":archived", AttributeValue::Bool(true));
                } else {
                    removes.push("archived");
                }

                let mut update_expr = format!("SET {}", sets.join(", "));
                if !removes.is_empty() {
                    update_expr.push_str(" REMOVE ");
                    update_expr.push_str(&removes.join(", "));
                }
                let resp = req
                    .update_expression(update_expr)
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Project {id}")))?;
                record_capacity("update_project", resp.consumed_capacity(), CapKind::Write);
            }
        }
        Ok(())
    }

    // ── billable_item ─────────────────────────────────────────────────────
    //
    // `date` is a DynamoDB reserved word: every expression that names it
    // aliases it as `#d`. (Raw attribute names in an `Item`/`Key`/
    // `ExclusiveStartKey` map are not expressions and need no alias.)

    async fn create_billable_item(
        &self,
        instance_id: &str,
        project_id: &str,
        date: &str,
        description: &str,
        quantity_hundredths: i64,
        unit_price_cents: i64,
        created_by_user_id: &str,
    ) -> db::Result<db::BillableItem> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("billable_item"))
            .item("id", AttributeValue::S(id.clone()))
            .item("instance_id", AttributeValue::S(instance_id.to_string()))
            .item("project_id", AttributeValue::S(project_id.to_string()))
            .item("date", AttributeValue::S(date.to_string()))
            .item("description", AttributeValue::S(description.to_string()))
            .item(
                "quantity_hundredths",
                AttributeValue::N(quantity_hundredths.to_string()),
            )
            .item(
                "unit_price_cents",
                AttributeValue::N(unit_price_cents.to_string()),
            )
            .item(
                "created_by_user_id",
                AttributeValue::S(created_by_user_id.to_string()),
            )
            .item("created_at", AttributeValue::N(now.to_string()))
            .item("updated_at", AttributeValue::N(now.to_string()))
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_billable_item",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(db::BillableItem {
            id,
            instance_id: instance_id.to_string(),
            project_id: project_id.to_string(),
            date: date.to_string(),
            description: description.to_string(),
            quantity_hundredths,
            unit_price_cents,
            invoice_id: None,
            created_by_user_id: created_by_user_id.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    async fn get_billable_items<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::BillableItem>>> {
        self.get_records("billable_item", ids).await
    }

    async fn get_billable_items_consistent<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::BillableItem>>> {
        self.get_records_consistent("billable_item", ids).await
    }

    async fn update_billable_item(
        &self,
        id: &str,
        invoice_id: Option<&str>,
        change: db::BillableItemUpdateShape<'_>,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let db::BillableItemUpdateShape::Fields {
            date,
            description,
            quantity_hundredths,
            unit_price_cents,
        } = change;
        let now = crate::clock::now_sec();

        let Some(invoice_id) = invoice_id else {
            // Unbilled item: unchanged from before invoices existed — a
            // plain conditional UpdateItem.
            let resp = self
                .client
                .update_item()
                .table_name(self.table_name("billable_item"))
                .key("id", AttributeValue::S(id.to_string()))
                .update_expression(
                    "SET #d = :date, description = :description, \
                     quantity_hundredths = :qty, unit_price_cents = :price, \
                     updated_at = :updated_at",
                )
                .condition_expression("attribute_exists(id) AND attribute_not_exists(invoice_id)")
                .expression_attribute_names("#d", "date")
                .expression_attribute_values(":date", AttributeValue::S(date.to_string()))
                .expression_attribute_values(
                    ":description",
                    AttributeValue::S(description.to_string()),
                )
                .expression_attribute_values(
                    ":qty",
                    AttributeValue::N(quantity_hundredths.to_string()),
                )
                .expression_attribute_values(
                    ":price",
                    AttributeValue::N(unit_price_cents.to_string()),
                )
                .expression_attribute_values(":updated_at", AttributeValue::N(now.to_string()))
                .return_consumed_capacity(ReturnConsumedCapacity::Total)
                .send()
                .await;
            return match resp {
                Ok(r) => {
                    record_capacity(
                        "update_billable_item",
                        r.consumed_capacity(),
                        CapKind::Write,
                    );
                    Ok(true)
                }
                Err(SdkError::ServiceError(ref se))
                    if se.err().is_conditional_check_failed_exception() =>
                {
                    Ok(false)
                }
                Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
            };
        };

        // The item is on a draft invoice (the caller has already rejected a
        // finalized one) — a TransactWriteItems that also bumps the
        // invoice's `version`, conditioned on it still being a draft. See
        // `db::Handler::update_billable_item`'s doc comment.
        let update_item = Update::builder()
            .table_name(self.table_name("billable_item"))
            .key("id", AttributeValue::S(id.to_string()))
            .update_expression(
                "SET #d = :date, description = :description, \
                 quantity_hundredths = :qty, unit_price_cents = :price, \
                 updated_at = :updated_at",
            )
            .condition_expression("attribute_exists(id) AND invoice_id = :invoice_id")
            .expression_attribute_names("#d", "date")
            .expression_attribute_values(":date", AttributeValue::S(date.to_string()))
            .expression_attribute_values(":description", AttributeValue::S(description.to_string()))
            .expression_attribute_values(":qty", AttributeValue::N(quantity_hundredths.to_string()))
            .expression_attribute_values(":price", AttributeValue::N(unit_price_cents.to_string()))
            .expression_attribute_values(":updated_at", AttributeValue::N(now.to_string()))
            .expression_attribute_values(":invoice_id", AttributeValue::S(invoice_id.to_string()))
            .build()
            .map_err(|e| db::Error::Infrastructure(e.to_string()))?;

        let update_invoice = Update::builder()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .update_expression("SET updated_at = :now ADD #v :one")
            .condition_expression("attribute_exists(id) AND #status = :draft")
            .expression_attribute_names("#status", "status")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .expression_attribute_values(":draft", AttributeValue::S("draft".to_string()))
            .build()
            .map_err(|e| db::Error::Infrastructure(e.to_string()))?;

        self.transact_write(
            "update_billable_item_on_draft_invoice",
            vec![
                TransactWriteItem::builder().update(update_item).build(),
                TransactWriteItem::builder().update(update_invoice).build(),
            ],
        )
        .await
    }

    async fn delete_billable_item(&self, id: &str) -> db::Result<bool> {
        self.ensure_writable()?;
        let resp = self
            .client
            .delete_item()
            .table_name(self.table_name("billable_item"))
            .key("id", AttributeValue::S(id.to_string()))
            .condition_expression("attribute_exists(id) AND attribute_not_exists(invoice_id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                record_capacity(
                    "delete_billable_item",
                    r.consumed_capacity(),
                    CapKind::Write,
                );
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se))
                if se.err().is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }

    async fn list_billable_items(
        &self,
        scope: db::BillableItemScope<'_>,
        filter: db::BillableItemFilter,
        page: db::ListBillableItemsPage,
    ) -> db::Result<Vec<db::BillableItem>> {
        let fetch_limit = usize::try_from(page.limit).unwrap_or(0);
        if fetch_limit == 0 {
            return Ok(Vec::new());
        }
        let (index_name, key_attr, key_value) = match scope {
            db::BillableItemScope::Instance(id) => ("instance_id-date-index", "instance_id", id),
            db::BillableItemScope::Project(id) => ("project_id-date-index", "project_id", id),
        };
        let filter_expression = match filter {
            db::BillableItemFilter::All => None,
            db::BillableItemFilter::Unbilled => Some("attribute_not_exists(invoice_id)"),
            db::BillableItemFilter::Billed => Some("attribute_exists(invoice_id)"),
        };

        // ExclusiveStartKey on a GSI must carry the table key (`id`) *and*
        // both index keys (the scope's hash attribute + `date`) — the same
        // rule `list_tickets` documents; the scope supplies the hash value,
        // the cursor supplies the rest.
        let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> =
            page.after.map(|c| {
                HashMap::from([
                    ("id".to_string(), AttributeValue::S(c.id)),
                    (
                        key_attr.to_string(),
                        AttributeValue::S(key_value.to_string()),
                    ),
                    ("date".to_string(), AttributeValue::S(c.date)),
                ])
            });

        // `Limit` bounds the rows DynamoDB *examines*, and the filter runs
        // after it — a filtered page can come back short (even empty) while
        // matching rows remain further down the index. Keep going until the
        // page is full or the index is exhausted.
        let mut items: Vec<db::BillableItem> = Vec::new();
        loop {
            let mut builder = self
                .client
                .query()
                .table_name(self.table_name("billable_item"))
                .index_name(index_name)
                .key_condition_expression(format!("{key_attr} = :key_value"))
                .expression_attribute_values(":key_value", AttributeValue::S(key_value.to_string()))
                .scan_index_forward(false)
                .return_consumed_capacity(ReturnConsumedCapacity::Total);
            // Unfiltered, `Limit` is exact. Filtered, a small `Limit` would
            // make a sparse filter walk the partition a page-size at a time,
            // so leave it off and let DynamoDB's 1 MB per-query cap bound
            // each round instead.
            match filter_expression {
                Some(f) => builder = builder.filter_expression(f),
                None => builder = builder.limit(page.limit),
            }
            if let Some(esk) = exclusive_start_key.take() {
                builder = builder.set_exclusive_start_key(Some(esk));
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
            record_capacity(
                "list_billable_items",
                resp.consumed_capacity(),
                CapKind::Read,
            );
            items.extend(hydrate_items::<db::BillableItem>(resp.items)?);
            exclusive_start_key = resp.last_evaluated_key;
            if items.len() >= fetch_limit || exclusive_start_key.is_none() {
                break;
            }
        }
        // A later round can overshoot; the caller's next cursor is built from
        // the last row actually returned, so dropping the surplus loses
        // nothing.
        items.truncate(fetch_limit);
        Ok(items)
    }

    // ── invoice ───────────────────────────────────────────────────────────
    //
    // `status`, `number`, `version`, and `snapshot` are DynamoDB reserved words: every
    // expression that names one aliases it (`#status`, `#num`, `#v`, `#snap`).

    async fn get_invoices<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::Invoice>>> {
        self.get_records("invoice", ids).await
    }

    async fn get_invoice_consistent(&self, id: &str) -> db::Result<Option<db::Invoice>> {
        self.get_record_consistent("get_invoice_consistent", "invoice", id)
            .await
    }

    async fn create_invoice(
        &self,
        instance_id: &str,
        project_id: &str,
        item_ids: &[String],
        created_by_user_id: &str,
    ) -> db::Result<Option<db::Invoice>> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();

        let mut invoice_item: HashMap<String, AttributeValue> = HashMap::from([
            ("id".to_string(), AttributeValue::S(id.clone())),
            (
                "instance_id".to_string(),
                AttributeValue::S(instance_id.to_string()),
            ),
            (
                "project_id".to_string(),
                AttributeValue::S(project_id.to_string()),
            ),
            ("status".to_string(), AttributeValue::S("draft".to_string())),
            ("version".to_string(), AttributeValue::N("1".to_string())),
            (
                "created_by_user_id".to_string(),
                AttributeValue::S(created_by_user_id.to_string()),
            ),
            ("created_at".to_string(), AttributeValue::N(now.to_string())),
            ("updated_at".to_string(), AttributeValue::N(now.to_string())),
        ]);
        if !item_ids.is_empty() {
            invoice_item.insert(
                "item_ids".to_string(),
                AttributeValue::Ss(item_ids.to_vec()),
            );
        }
        let put_invoice = Put::builder()
            .table_name(self.table_name("invoice"))
            .set_item(Some(invoice_item))
            .condition_expression("attribute_not_exists(id)")
            .build()
            .map_err(|e| db::Error::Infrastructure(e.to_string()))?;

        let mut transact_items = vec![TransactWriteItem::builder().put(put_invoice).build()];
        for item_id in item_ids {
            let update = Update::builder()
                .table_name(self.table_name("billable_item"))
                .key("id", AttributeValue::S(item_id.clone()))
                .update_expression("SET invoice_id = :invoice_id")
                .condition_expression(
                    "attribute_exists(id) AND attribute_not_exists(invoice_id) AND project_id = :project_id",
                )
                .expression_attribute_values(":invoice_id", AttributeValue::S(id.clone()))
                .expression_attribute_values(
                    ":project_id",
                    AttributeValue::S(project_id.to_string()),
                )
                .build()
                .map_err(|e| db::Error::Infrastructure(e.to_string()))?;
            transact_items.push(TransactWriteItem::builder().update(update).build());
        }

        let committed = self
            .transact_write("create_invoice", transact_items)
            .await?;
        if !committed {
            return Ok(None);
        }
        Ok(Some(db::Invoice {
            id,
            instance_id: instance_id.to_string(),
            project_id: project_id.to_string(),
            status: db::InvoiceStatus::Draft,
            version: 1,
            item_ids: item_ids.to_vec(),
            created_by_user_id: created_by_user_id.to_string(),
            created_at: now,
            updated_at: now,
            number: None,
            issue_date: None,
            snapshot: None,
            total_cents: None,
            finalized_at: None,
            finalized_by_user_id: None,
            paid_date: None,
            pdf_s3_key: None,
        }))
    }

    async fn add_invoice_items(
        &self,
        invoice_id: &str,
        project_id: &str,
        item_ids: &[String],
        expected_version: u64,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let update_invoice = Update::builder()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .update_expression("SET updated_at = :now ADD item_ids :ids, #v :one")
            .condition_expression("#status = :draft AND #v = :expected_version")
            .expression_attribute_names("#status", "status")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
            .expression_attribute_values(":ids", AttributeValue::Ss(item_ids.to_vec()))
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .expression_attribute_values(":draft", AttributeValue::S("draft".to_string()))
            .expression_attribute_values(
                ":expected_version",
                AttributeValue::N(expected_version.to_string()),
            )
            .build()
            .map_err(|e| db::Error::Infrastructure(e.to_string()))?;

        let mut transact_items = vec![TransactWriteItem::builder().update(update_invoice).build()];
        for item_id in item_ids {
            let update = Update::builder()
                .table_name(self.table_name("billable_item"))
                .key("id", AttributeValue::S(item_id.clone()))
                .update_expression("SET invoice_id = :invoice_id")
                .condition_expression(
                    "attribute_exists(id) AND attribute_not_exists(invoice_id) AND project_id = :project_id",
                )
                .expression_attribute_values(
                    ":invoice_id",
                    AttributeValue::S(invoice_id.to_string()),
                )
                .expression_attribute_values(
                    ":project_id",
                    AttributeValue::S(project_id.to_string()),
                )
                .build()
                .map_err(|e| db::Error::Infrastructure(e.to_string()))?;
            transact_items.push(TransactWriteItem::builder().update(update).build());
        }
        self.transact_write("add_invoice_items", transact_items)
            .await
    }

    async fn remove_invoice_items(
        &self,
        invoice_id: &str,
        item_ids: &[String],
        expected_version: u64,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let update_invoice = Update::builder()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .update_expression("SET updated_at = :now ADD #v :one DELETE item_ids :ids")
            .condition_expression("#status = :draft AND #v = :expected_version")
            .expression_attribute_names("#status", "status")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
            .expression_attribute_values(":ids", AttributeValue::Ss(item_ids.to_vec()))
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .expression_attribute_values(":draft", AttributeValue::S("draft".to_string()))
            .expression_attribute_values(
                ":expected_version",
                AttributeValue::N(expected_version.to_string()),
            )
            .build()
            .map_err(|e| db::Error::Infrastructure(e.to_string()))?;

        let mut transact_items = vec![TransactWriteItem::builder().update(update_invoice).build()];
        for item_id in item_ids {
            let update = Update::builder()
                .table_name(self.table_name("billable_item"))
                .key("id", AttributeValue::S(item_id.clone()))
                .update_expression("REMOVE invoice_id")
                .condition_expression("attribute_exists(id) AND invoice_id = :invoice_id")
                .expression_attribute_values(
                    ":invoice_id",
                    AttributeValue::S(invoice_id.to_string()),
                )
                .build()
                .map_err(|e| db::Error::Infrastructure(e.to_string()))?;
            transact_items.push(TransactWriteItem::builder().update(update).build());
        }
        self.transact_write("remove_invoice_items", transact_items)
            .await
    }

    async fn delete_invoice(
        &self,
        invoice_id: &str,
        item_ids: &[String],
        expected_version: u64,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let delete_invoice = Delete::builder()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .condition_expression("#status = :draft AND #v = :expected_version")
            .expression_attribute_names("#status", "status")
            .expression_attribute_names("#v", "version")
            .expression_attribute_values(":draft", AttributeValue::S("draft".to_string()))
            .expression_attribute_values(
                ":expected_version",
                AttributeValue::N(expected_version.to_string()),
            )
            .build()
            .map_err(|e| db::Error::Infrastructure(e.to_string()))?;

        let mut transact_items = vec![TransactWriteItem::builder().delete(delete_invoice).build()];
        for item_id in item_ids {
            let update = Update::builder()
                .table_name(self.table_name("billable_item"))
                .key("id", AttributeValue::S(item_id.clone()))
                .update_expression("REMOVE invoice_id")
                .condition_expression("attribute_exists(id) AND invoice_id = :invoice_id")
                .expression_attribute_values(
                    ":invoice_id",
                    AttributeValue::S(invoice_id.to_string()),
                )
                .build()
                .map_err(|e| db::Error::Infrastructure(e.to_string()))?;
            transact_items.push(TransactWriteItem::builder().update(update).build());
        }
        self.transact_write("delete_invoice", transact_items).await
    }

    async fn finalize_invoice(
        &self,
        invoice_id: &str,
        expected_version: u64,
        number: u32,
        issue_date: &str,
        snapshot_json: &str,
        total_cents: i64,
        finalized_by_user_id: &str,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let resp = self
            .client
            .update_item()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .update_expression(
                "SET #status = :finalized, #num = :number, issue_date = :issue_date, \
                 #snap = :snapshot, total_cents = :total_cents, \
                 finalized_at = :now, finalized_by_user_id = :finalized_by, \
                 updated_at = :now ADD #v :one",
            )
            .condition_expression(
                "attribute_exists(id) AND #status = :draft AND #v = :expected_version",
            )
            .expression_attribute_names("#status", "status")
            .expression_attribute_names("#num", "number")
            .expression_attribute_names("#v", "version")
            // `snapshot` is a DynamoDB reserved word too.
            .expression_attribute_names("#snap", "snapshot")
            .expression_attribute_values(":finalized", AttributeValue::S("finalized".to_string()))
            .expression_attribute_values(":draft", AttributeValue::S("draft".to_string()))
            .expression_attribute_values(
                ":expected_version",
                AttributeValue::N(expected_version.to_string()),
            )
            .expression_attribute_values(":number", AttributeValue::N(number.to_string()))
            .expression_attribute_values(":issue_date", AttributeValue::S(issue_date.to_string()))
            .expression_attribute_values(":snapshot", AttributeValue::S(snapshot_json.to_string()))
            .expression_attribute_values(":total_cents", AttributeValue::N(total_cents.to_string()))
            .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .expression_attribute_values(
                ":finalized_by",
                AttributeValue::S(finalized_by_user_id.to_string()),
            )
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                record_capacity("finalize_invoice", r.consumed_capacity(), CapKind::Write);
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se))
                if se.err().is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }

    async fn set_invoice_paid(
        &self,
        invoice_id: &str,
        paid_date: Option<&str>,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let mut req = self
            .client
            .update_item()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .condition_expression("attribute_exists(id) AND #status = :finalized")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":finalized", AttributeValue::S("finalized".to_string()))
            .expression_attribute_values(":now", AttributeValue::N(now.to_string()));
        req = match paid_date {
            Some(d) => req
                .update_expression("SET paid_date = :paid_date, updated_at = :now")
                .expression_attribute_values(":paid_date", AttributeValue::S(d.to_string())),
            None => req.update_expression("REMOVE paid_date SET updated_at = :now"),
        };
        let resp = req
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                record_capacity("set_invoice_paid", r.consumed_capacity(), CapKind::Write);
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se))
                if se.err().is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }

    async fn set_invoice_pdf_key(&self, invoice_id: &str, key: &str) -> db::Result<bool> {
        self.ensure_writable()?;
        let resp = self
            .client
            .update_item()
            .table_name(self.table_name("invoice"))
            .key("id", AttributeValue::S(invoice_id.to_string()))
            .update_expression("SET pdf_s3_key = :key")
            .condition_expression("attribute_exists(id) AND #status = :finalized")
            .expression_attribute_names("#status", "status")
            .expression_attribute_values(":key", AttributeValue::S(key.to_string()))
            .expression_attribute_values(":finalized", AttributeValue::S("finalized".to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                record_capacity("set_invoice_pdf_key", r.consumed_capacity(), CapKind::Write);
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se))
                if se.err().is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }

    async fn increment_invoice_counter(&self, instance_id: &str) -> db::Result<u64> {
        self.ensure_writable()?;
        let resp = self
            .client
            .update_item()
            .table_name(self.table_name("counter"))
            .key("id", AttributeValue::S(instance_id.to_string()))
            .update_expression("ADD next_invoice_number :one")
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .return_values(ReturnValue::UpdatedNew)
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "increment_invoice_counter",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        let attrs = resp.attributes.ok_or_else(|| {
            db::Error::Infrastructure("counter update returned no attributes".into())
        })?;
        let n = attrs
            .get("next_invoice_number")
            .and_then(|v| v.as_n().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                db::Error::Infrastructure("counter update missing next_invoice_number".into())
            })?;
        Ok(n)
    }

    async fn get_invoice_counter(&self, instance_id: &str) -> db::Result<u64> {
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("counter"))
            .key("id", AttributeValue::S(instance_id.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_invoice_counter",
            resp.consumed_capacity(),
            CapKind::Read,
        );
        Ok(resp
            .item
            .as_ref()
            .and_then(|item| item.get("next_invoice_number"))
            .and_then(|v| v.as_n().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0))
    }

    async fn set_next_invoice_number(&self, instance_id: &str, new_value: u64) -> db::Result<bool> {
        self.ensure_writable()?;
        let resp = self
            .client
            .update_item()
            .table_name(self.table_name("counter"))
            .key("id", AttributeValue::S(instance_id.to_string()))
            .update_expression("SET next_invoice_number = :new_value")
            .condition_expression(
                "attribute_not_exists(next_invoice_number) OR next_invoice_number <= :new_value",
            )
            .expression_attribute_values(":new_value", AttributeValue::N(new_value.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                record_capacity(
                    "set_next_invoice_number",
                    r.consumed_capacity(),
                    CapKind::Write,
                );
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se))
                if se.err().is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }

    async fn list_invoices(
        &self,
        scope: db::InvoiceScope<'_>,
        filter: db::InvoiceListFilter,
        page: db::ListInvoicesPage,
    ) -> db::Result<Vec<db::Invoice>> {
        let fetch_limit = usize::try_from(page.limit).unwrap_or(0);
        if fetch_limit == 0 {
            return Ok(Vec::new());
        }
        let (index_name, key_attr, key_value) = match scope {
            db::InvoiceScope::Instance(id) => ("instance_id-created_at-index", "instance_id", id),
            db::InvoiceScope::Project(id) => ("project_id-created_at-index", "project_id", id),
        };
        let filter_expression = match filter {
            db::InvoiceListFilter::All => None,
            db::InvoiceListFilter::Draft => Some("#status = :status_value"),
            db::InvoiceListFilter::Unpaid => {
                Some("#status = :status_value AND attribute_not_exists(paid_date)")
            }
            db::InvoiceListFilter::Paid => {
                Some("#status = :status_value AND attribute_exists(paid_date)")
            }
        };
        let status_value = match filter {
            db::InvoiceListFilter::All => None,
            db::InvoiceListFilter::Draft => Some("draft"),
            db::InvoiceListFilter::Unpaid | db::InvoiceListFilter::Paid => Some("finalized"),
        };

        let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> =
            page.after.map(|c| {
                HashMap::from([
                    ("id".to_string(), AttributeValue::S(c.id)),
                    (
                        key_attr.to_string(),
                        AttributeValue::S(key_value.to_string()),
                    ),
                    (
                        "created_at".to_string(),
                        AttributeValue::N(c.created_at.to_string()),
                    ),
                ])
            });

        let mut items: Vec<db::Invoice> = Vec::new();
        loop {
            let mut builder = self
                .client
                .query()
                .table_name(self.table_name("invoice"))
                .index_name(index_name)
                .key_condition_expression(format!("{key_attr} = :key_value"))
                .expression_attribute_values(":key_value", AttributeValue::S(key_value.to_string()))
                .scan_index_forward(false)
                .return_consumed_capacity(ReturnConsumedCapacity::Total);
            match filter_expression {
                Some(f) => {
                    builder = builder
                        .filter_expression(f)
                        .expression_attribute_names("#status", "status");
                    if let Some(v) = status_value {
                        builder = builder.expression_attribute_values(
                            ":status_value",
                            AttributeValue::S(v.to_string()),
                        );
                    }
                }
                None => builder = builder.limit(page.limit),
            }
            if let Some(esk) = exclusive_start_key.take() {
                builder = builder.set_exclusive_start_key(Some(esk));
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
            record_capacity("list_invoices", resp.consumed_capacity(), CapKind::Read);
            items.extend(hydrate_items::<db::Invoice>(resp.items)?);
            exclusive_start_key = resp.last_evaluated_key;
            if items.len() >= fetch_limit || exclusive_start_key.is_none() {
                break;
            }
        }
        items.truncate(fetch_limit);
        Ok(items)
    }

    // ── ticket ────────────────────────────────────────────────────────────

    async fn get_tickets<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> db::Result<Vec<Option<db::Ticket>>> {
        self.get_records("ticket", ids).await
    }

    async fn increment_ticket_counter(&self, instance_id: &str) -> db::Result<u64> {
        self.ensure_writable()?;
        let resp = self
            .client
            .update_item()
            .table_name(self.table_name("counter"))
            .key("id", AttributeValue::S(instance_id.to_string()))
            .update_expression("ADD next_ticket_number :one")
            .expression_attribute_values(":one", AttributeValue::N("1".to_string()))
            .return_values(ReturnValue::UpdatedNew)
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "increment_ticket_counter",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        let attrs = resp.attributes.ok_or_else(|| {
            db::Error::Infrastructure("counter update returned no attributes".into())
        })?;
        let n = attrs
            .get("next_ticket_number")
            .and_then(|v| v.as_n().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                db::Error::Infrastructure("counter update missing next_ticket_number".into())
            })?;
        Ok(n)
    }

    async fn create_ticket(
        &self,
        instance_id: &str,
        number: u64,
        subject: &str,
        requester_emails: &[String],
        cc_emails: &[String],
    ) -> db::Result<db::Ticket> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();
        let reply_token = nanoid!(16, &NANOID_ALPHABET);
        // A freshly created ticket is always Open and unassigned — the only
        // caller of compute_ticket_markers that doesn't come from
        // TicketUpdateShape::SetStatusAndAssignee, since there is no prior
        // marker state to reconcile against yet.
        let markers = db::compute_ticket_markers(instance_id, db::TicketStatus::Open, None);

        let mut req = self
            .client
            .put_item()
            .table_name(self.table_name("ticket"))
            .item("id", AttributeValue::S(id.clone()))
            .item("instance_id", AttributeValue::S(instance_id.to_string()))
            .item("number", AttributeValue::N(number.to_string()))
            .item("subject", AttributeValue::S(subject.to_string()))
            .item(
                "status",
                AttributeValue::S(db::TicketStatus::Open.as_str().to_string()),
            )
            .item(
                "instance_status",
                AttributeValue::S(markers.instance_status.clone()),
            )
            .item(
                "instance_number",
                AttributeValue::S(format!("{instance_id}#{number}")),
            )
            .item("reply_token", AttributeValue::S(reply_token.clone()))
            .item("created_at", AttributeValue::N(now.to_string()))
            .item("updated_at", AttributeValue::N(now.to_string()))
            .item("last_activity_at", AttributeValue::N(now.to_string()))
            // Guards against an astronomically unlikely nanoid collision, same
            // convention as create_instance/create_user_token.
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total);
        if let Some(v) = &markers.instance_visible {
            req = req.item("instance_visible", AttributeValue::S(v.clone()));
        }
        // instance_assignee is never written at creation — a new ticket is
        // always unassigned, so compute_ticket_markers(.., None) already
        // omits it.
        if !requester_emails.is_empty() {
            req = req.item(
                "requester_emails",
                AttributeValue::Ss(requester_emails.to_vec()),
            );
        }
        if !cc_emails.is_empty() {
            req = req.item("cc_emails", AttributeValue::Ss(cc_emails.to_vec()));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity("create_ticket", resp.consumed_capacity(), CapKind::Write);

        Ok(db::Ticket {
            id,
            instance_id: instance_id.to_string(),
            number,
            subject: subject.to_string(),
            status: db::TicketStatus::Open,
            requester_emails: requester_emails.to_vec(),
            cc_emails: cc_emails.to_vec(),
            assignee_user_id: None,
            reply_token,
            created_at: now,
            updated_at: now,
            last_activity_at: now,
            has_attachments: false,
        })
    }

    async fn update_ticket(&self, id: &str, change: db::TicketUpdateShape<'_>) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::TicketUpdateShape::SetStatusAndAssignee {
                instance_id,
                status,
                assignee_user_id,
                now,
            } => {
                let markers = db::compute_ticket_markers(instance_id, status, assignee_user_id);

                let mut sets = vec![
                    "#status = :status".to_string(),
                    "updated_at = :now".to_string(),
                    "last_activity_at = :now".to_string(),
                    "instance_status = :instance_status".to_string(),
                ];
                let mut removes: Vec<&str> = Vec::new();

                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .expression_attribute_names("#status", "status")
                    .expression_attribute_values(
                        ":status",
                        AttributeValue::S(status.as_str().to_string()),
                    )
                    .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                    .expression_attribute_values(
                        ":instance_status",
                        AttributeValue::S(markers.instance_status),
                    );

                // assignee_user_id (the plain field, not the instance_assignee
                // GSI marker) always reflects the caller's desired assignee,
                // independent of status — a deleted-but-assigned ticket keeps
                // its assignee_user_id so a restore doesn't lose it, even
                // though instance_assignee is dropped below.
                match assignee_user_id {
                    Some(assignee) => {
                        sets.push("assignee_user_id = :assignee".to_string());
                        req = req.expression_attribute_values(
                            ":assignee",
                            AttributeValue::S(assignee.to_string()),
                        );
                    }
                    None => removes.push("assignee_user_id"),
                }

                match markers.instance_visible {
                    Some(v) => {
                        sets.push("instance_visible = :instance_visible".to_string());
                        req = req
                            .expression_attribute_values(":instance_visible", AttributeValue::S(v));
                    }
                    None => removes.push("instance_visible"),
                }
                match markers.instance_assignee {
                    Some(v) => {
                        sets.push("instance_assignee = :instance_assignee".to_string());
                        req = req.expression_attribute_values(
                            ":instance_assignee",
                            AttributeValue::S(v),
                        );
                    }
                    None => removes.push("instance_assignee"),
                }

                let mut update_expr = format!("SET {}", sets.join(", "));
                if !removes.is_empty() {
                    update_expr.push_str(&format!(" REMOVE {}", removes.join(", ")));
                }

                let resp = req
                    .update_expression(update_expr)
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_status_assignee",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketUpdateShape::AddRequester { email, now } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression(
                        "SET updated_at = :now, last_activity_at = :now ADD requester_emails :emails",
                    )
                    .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                    .expression_attribute_values(
                        ":emails",
                        AttributeValue::Ss(vec![email.to_string()]),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_add_requester",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketUpdateShape::RemoveRequester { email, now } => {
                // DynamoDB removes a String Set attribute entirely once its
                // last element is DELETEd — exactly the omit-don't-null
                // behaviour the house rule wants, with no extra code needed
                // here.
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression(
                        "SET updated_at = :now, last_activity_at = :now DELETE requester_emails :emails",
                    )
                    .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                    .expression_attribute_values(
                        ":emails",
                        AttributeValue::Ss(vec![email.to_string()]),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_remove_requester",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketUpdateShape::AddCc { email, now } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression(
                        "SET updated_at = :now, last_activity_at = :now ADD cc_emails :emails",
                    )
                    .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                    .expression_attribute_values(
                        ":emails",
                        AttributeValue::Ss(vec![email.to_string()]),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_add_cc",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketUpdateShape::RemoveCc { email, now } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression(
                        "SET updated_at = :now, last_activity_at = :now DELETE cc_emails :emails",
                    )
                    .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                    .expression_attribute_values(
                        ":emails",
                        AttributeValue::Ss(vec![email.to_string()]),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_remove_cc",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketUpdateShape::MarkHasAttachments => {
                // Written only when true, never as `false` — the project's
                // omit-don't-null rule. A ticket with no attachments simply has
                // no such attribute, and hydration reads that as false.
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET has_attachments = :t")
                    .expression_attribute_values(":t", AttributeValue::Bool(true))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_mark_has_attachments",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketUpdateShape::Touch { now } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET updated_at = :now, last_activity_at = :now")
                    .expression_attribute_values(":now", AttributeValue::N(now.to_string()))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("Ticket {id}")))?;
                record_capacity(
                    "update_ticket_touch",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn list_tickets(
        &self,
        instance_id: &str,
        filter: db::TicketListFilter,
        page: db::ListTicketsPage,
    ) -> db::Result<Vec<db::Ticket>> {
        let fetch_limit = page.limit as usize;
        let (scan_forward, reverse_output) =
            page_scan_direction(page.after.is_some(), page.before.is_some(), page.descending);

        let (index_name, key_attr, key_value, status_filter_value): (
            &str,
            &str,
            String,
            Option<String>,
        ) = match &filter {
            db::TicketListFilter::Status(status) => (
                "instance_status-last_activity_at-index",
                "instance_status",
                format!("{instance_id}#{}", status.as_str()),
                None,
            ),
            db::TicketListFilter::Visible => (
                "instance_visible-last_activity_at-index",
                "instance_visible",
                instance_id.to_string(),
                None,
            ),
            db::TicketListFilter::AssignedTo { user_id, status } => (
                "instance_assignee-last_activity_at-index",
                "instance_assignee",
                format!("{instance_id}#{user_id}"),
                status.as_ref().map(|s| s.as_str().to_string()),
            ),
        };

        // Initial ExclusiveStartKey from the caller's cursor. Must carry the
        // table hash key (id) *and* both GSI keys (the queried GSI's hash
        // attribute + last_activity_at) — see the seslogin periods reference
        // this mirrors; omitting either silently truncates the page instead
        // of erroring.
        let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> =
            page.after.as_ref().or(page.before.as_ref()).map(|c| {
                HashMap::from([
                    ("id".to_string(), AttributeValue::S(c.id.clone())),
                    (key_attr.to_string(), AttributeValue::S(key_value.clone())),
                    (
                        "last_activity_at".to_string(),
                        AttributeValue::N(c.last_activity_at.to_string()),
                    ),
                ])
            });

        let mut tickets: Vec<db::Ticket> = Vec::new();
        loop {
            let mut builder = self
                .client
                .query()
                .table_name(self.table_name("ticket"))
                .index_name(index_name)
                .key_condition_expression(format!("{key_attr} = :key_value"))
                .expression_attribute_values(":key_value", AttributeValue::S(key_value.clone()))
                .limit(page.limit)
                .scan_index_forward(scan_forward)
                .return_consumed_capacity(ReturnConsumedCapacity::Total);
            if let Some(status_value) = &status_filter_value {
                builder = builder
                    .filter_expression("#status = :status_value")
                    .expression_attribute_names("#status", "status")
                    .expression_attribute_values(
                        ":status_value",
                        AttributeValue::S(status_value.clone()),
                    );
            }
            if let Some(esk) = exclusive_start_key.take() {
                builder = builder.set_exclusive_start_key(Some(esk));
            }

            let resp = builder
                .send()
                .await
                .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
            record_capacity("list_tickets", resp.consumed_capacity(), CapKind::Read);
            tickets.extend(hydrate_items::<db::Ticket>(resp.items)?);
            exclusive_start_key = resp.last_evaluated_key;

            if tickets.len() >= fetch_limit || exclusive_start_key.is_none() {
                break;
            }
        }

        if reverse_output {
            tickets.reverse();
        }
        Ok(tickets)
    }

    async fn get_ticket_id_by_instance_number(
        &self,
        instance_id: &str,
        number: u64,
    ) -> db::Result<Option<String>> {
        let key_value = format!("{instance_id}#{number}");
        let resp = self
            .client
            .query()
            .table_name(self.table_name("ticket"))
            .index_name("instance_number-index")
            .key_condition_expression("instance_number = :v")
            .expression_attribute_values(":v", AttributeValue::S(key_value.clone()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_ticket_id_by_instance_number",
            resp.consumed_capacity(),
            CapKind::Read,
        );
        let ids = resp
            .items
            .unwrap_or_default()
            .into_iter()
            .map(|item| Item(item).id())
            .collect::<HydrationResult<Vec<String>>>()?;
        db::at_most_one(ids, || {
            format!("Multiple tickets share instance_number {key_value}")
        })
    }

    // ── ticket_message ───────────────────────────────────────────────────

    async fn create_ticket_message(
        &self,
        ticket_id: &str,
        kind: db::TicketMessageKind,
        author_user_id: Option<&str>,
        from_email: Option<&str>,
        to_emails: &[String],
        cc_emails: &[String],
        body_text: Option<&str>,
        body_html: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
    ) -> db::Result<db::TicketMessage> {
        self.ensure_writable()?;
        let id = new_id();
        let now = crate::clock::now_sec();

        let mut req = self
            .client
            .put_item()
            .table_name(self.table_name("ticket_message"))
            .item("id", AttributeValue::S(id.clone()))
            .item("ticket_id", AttributeValue::S(ticket_id.to_string()))
            .item("kind", AttributeValue::S(kind.as_str().to_string()))
            .item("created_at", AttributeValue::N(now.to_string()))
            .condition_expression("attribute_not_exists(id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total);
        if let Some(uid) = author_user_id {
            req = req.item("author_user_id", AttributeValue::S(uid.to_string()));
        }
        if let Some(email) = from_email {
            req = req.item("from_email", AttributeValue::S(email.to_string()));
        }
        if !to_emails.is_empty() {
            req = req.item("to_emails", AttributeValue::Ss(to_emails.to_vec()));
        }
        if !cc_emails.is_empty() {
            req = req.item("cc_emails", AttributeValue::Ss(cc_emails.to_vec()));
        }
        if let Some(t) = body_text {
            req = req.item("body_text", AttributeValue::S(t.to_string()));
        }
        if let Some(h) = body_html {
            req = req.item("body_html", AttributeValue::S(h.to_string()));
        }
        if let Some(irt) = in_reply_to {
            req = req.item("in_reply_to", AttributeValue::S(irt.to_string()));
        }
        if let Some(refs) = references {
            req = req.item("references", AttributeValue::S(refs.to_string()));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_ticket_message",
            resp.consumed_capacity(),
            CapKind::Write,
        );

        Ok(db::TicketMessage {
            id,
            ticket_id: ticket_id.to_string(),
            kind,
            author_user_id: author_user_id.map(String::from),
            from_email: from_email.map(String::from),
            to_emails: to_emails.to_vec(),
            cc_emails: cc_emails.to_vec(),
            body_text: body_text.map(String::from),
            body_html: body_html.map(String::from),
            rfc_message_id: None,
            in_reply_to: in_reply_to.map(String::from),
            references: references.map(String::from),
            attachments: vec![],
            raw_s3_key: None,
            created_at: now,
        })
    }

    async fn list_ticket_messages(&self, ticket_id: &str) -> db::Result<Vec<db::TicketMessage>> {
        let mut messages: Vec<db::TicketMessage> = query_all("list_ticket_messages", || {
            self.client
                .query()
                .table_name(self.table_name("ticket_message"))
                .index_name("ticket_id-created_at-index")
                .key_condition_expression("ticket_id = :ticket_id")
                .expression_attribute_values(":ticket_id", AttributeValue::S(ticket_id.to_string()))
        })
        .await?;
        // `created_at` is in whole seconds, so the GSI's sort key ties for any
        // two messages written in the same second — an inbound message and the
        // notification it triggers, typically — and DynamoDB is free to return
        // tied rows in any order. That surfaced as a thread whose order changed
        // between reads. Breaking the tie on `id` makes the order *stable*;
        // within one second it is not true insertion order, which is recorded
        // as a known issue in SCHEMA.md along with the fix (a millisecond sort
        // key). Stable-but-arbitrary beats non-deterministic: a reader never
        // sees the same thread reshuffle.
        messages.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(messages)
    }

    async fn update_ticket_message(
        &self,
        id: &str,
        change: db::TicketMessageUpdateShape<'_>,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::TicketMessageUpdateShape::SetRfcMessageId { rfc_message_id } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket_message"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET rfc_message_id = :rfc_message_id")
                    .expression_attribute_values(
                        ":rfc_message_id",
                        AttributeValue::S(rfc_message_id.to_string()),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("TicketMessage {id}")))?;
                record_capacity(
                    "update_ticket_message_set_rfc_message_id",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketMessageUpdateShape::SetRawS3Key { raw_s3_key } => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket_message"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET raw_s3_key = :raw_s3_key")
                    .expression_attribute_values(
                        ":raw_s3_key",
                        AttributeValue::S(raw_s3_key.to_string()),
                    )
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("TicketMessage {id}")))?;
                record_capacity(
                    "update_ticket_message_set_raw_s3_key",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::TicketMessageUpdateShape::SetAttachments { attachments } => {
                let mut req = self
                    .client
                    .update_item()
                    .table_name(self.table_name("ticket_message"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)");
                if attachments.is_empty() {
                    req = req.update_expression("REMOVE attachments");
                } else {
                    let list = AttributeValue::L(
                        attachments
                            .iter()
                            .map(|a| {
                                AttributeValue::M(HashMap::from([
                                    ("s3_key".to_string(), AttributeValue::S(a.s3_key.clone())),
                                    (
                                        "filename".to_string(),
                                        AttributeValue::S(a.filename.clone()),
                                    ),
                                    (
                                        "content_type".to_string(),
                                        AttributeValue::S(a.content_type.clone()),
                                    ),
                                    ("size".to_string(), AttributeValue::N(a.size.to_string())),
                                ]))
                            })
                            .collect(),
                    );
                    req = req
                        .update_expression("SET attachments = :attachments")
                        .expression_attribute_values(":attachments", list);
                }
                let resp = req
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("TicketMessage {id}")))?;
                record_capacity(
                    "update_ticket_message_set_attachments",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn get_ticket_id_by_rfc_message_id(
        &self,
        rfc_message_id: &str,
    ) -> db::Result<Option<String>> {
        let resp = self
            .client
            .query()
            .table_name(self.table_name("ticket_message"))
            .index_name("rfc_message_id-index")
            .key_condition_expression("rfc_message_id = :v")
            .expression_attribute_values(":v", AttributeValue::S(rfc_message_id.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_ticket_id_by_rfc_message_id",
            resp.consumed_capacity(),
            CapKind::Read,
        );
        let message_ids = resp
            .items
            .unwrap_or_default()
            .into_iter()
            .map(|item| Item(item).id())
            .collect::<HydrationResult<Vec<String>>>()?;
        let Some(message_id) = db::at_most_one(message_ids, || {
            format!("Multiple ticket_messages share rfc_message_id {rfc_message_id}")
        })?
        else {
            return Ok(None);
        };

        // Strongly consistent GetItem on the message itself, to read
        // ticket_id back out — the index only projects the message's own id.
        let messages: Vec<Option<db::TicketMessage>> = self
            .get_records("ticket_message", &[message_id.as_str()])
            .await?;
        Ok(messages.into_iter().next().flatten().map(|m| m.ticket_id))
    }

    // ── processed_message ─────────────────────────────────────────────────

    async fn claim_processed_message(
        &self,
        ses_message_id: &str,
        now: u64,
        expires_at: u64,
    ) -> db::Result<bool> {
        self.ensure_writable()?;
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("processed_message"))
            .item(
                "ses_message_id",
                AttributeValue::S(ses_message_id.to_string()),
            )
            .item("processed_at", AttributeValue::N(now.to_string()))
            .item("expires_at", AttributeValue::N(expires_at.to_string()))
            .condition_expression("attribute_not_exists(ses_message_id)")
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await;
        match resp {
            Ok(r) => {
                record_capacity(
                    "claim_processed_message",
                    r.consumed_capacity(),
                    CapKind::Write,
                );
                Ok(true)
            }
            Err(SdkError::ServiceError(ref se))
                if se.err().is_conditional_check_failed_exception() =>
            {
                Ok(false)
            }
            Err(e) => Err(db::Error::Infrastructure(sdk_err_msg(e))),
        }
    }

    // ── webauthn_credential ───────────────────────────────────────────────

    async fn create_webauthn_credential(
        &self,
        id: &str,
        user_id: &str,
        name: &str,
        passkey_json: &str,
    ) -> db::Result<db::WebauthnCredential> {
        self.ensure_writable()?;
        let now = crate::clock::now_sec();
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("webauthn_credential"))
            .item("id", AttributeValue::S(id.to_string()))
            .item("user_id", AttributeValue::S(user_id.to_string()))
            .item("name", AttributeValue::S(name.to_string()))
            .item("passkey", AttributeValue::S(passkey_json.to_string()))
            .item("created_at", AttributeValue::N(now.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "create_webauthn_credential",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(db::WebauthnCredential {
            id: id.to_string(),
            user_id: user_id.to_string(),
            name: name.to_string(),
            passkey_json: passkey_json.to_string(),
            created_at: now,
            last_used_at: None,
        })
    }

    async fn get_webauthn_credential(
        &self,
        id: &str,
    ) -> db::Result<Option<db::WebauthnCredential>> {
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("webauthn_credential"))
            .key("id", AttributeValue::S(id.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_webauthn_credential",
            resp.consumed_capacity(),
            CapKind::Read,
        );
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn list_webauthn_credentials_by_user(
        &self,
        user_id: &str,
    ) -> db::Result<Vec<db::WebauthnCredential>> {
        query_all("list_webauthn_credentials_by_user", || {
            self.client
                .query()
                .table_name(self.table_name("webauthn_credential"))
                .index_name("user_id-index")
                .key_condition_expression("user_id = :user_id")
                .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_string()))
        })
        .await
    }

    async fn count_webauthn_credentials_by_user(&self, user_id: &str) -> db::Result<usize> {
        // `Select::Count` still reports a per-page count alongside a continuation
        // key, so sum across pages rather than trusting the first response.
        let mut total: usize = 0;
        let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> = None;

        loop {
            let mut builder = self
                .client
                .query()
                .table_name(self.table_name("webauthn_credential"))
                .index_name("user_id-index")
                .key_condition_expression("user_id = :user_id")
                .expression_attribute_values(":user_id", AttributeValue::S(user_id.to_string()))
                .select(aws_sdk_dynamodb::types::Select::Count)
                .return_consumed_capacity(ReturnConsumedCapacity::Total);
            if let Some(esk) = exclusive_start_key.take() {
                builder = builder.set_exclusive_start_key(Some(esk));
            }

            let resp = builder
                .send()
                .await
                .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
            record_capacity(
                "count_webauthn_credentials_by_user",
                resp.consumed_capacity(),
                CapKind::Read,
            );

            total += resp.count.max(0) as usize;
            exclusive_start_key = resp.last_evaluated_key;
            if exclusive_start_key.is_none() {
                return Ok(total);
            }
        }
    }

    async fn update_webauthn_credential(
        &self,
        id: &str,
        change: db::WebauthnCredentialUpdate,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        match change {
            db::WebauthnCredentialUpdate::Rename(name) => {
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("webauthn_credential"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET #n = :name")
                    .expression_attribute_names("#n", "name")
                    .expression_attribute_values(":name", AttributeValue::S(name))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("WebauthnCredential {id}")))?;
                record_capacity(
                    "update_webauthn_credential_rename",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
            db::WebauthnCredentialUpdate::TouchLastUsed { passkey_json } => {
                let now = crate::clock::now_sec();
                let resp = self
                    .client
                    .update_item()
                    .table_name(self.table_name("webauthn_credential"))
                    .key("id", AttributeValue::S(id.to_string()))
                    .condition_expression("attribute_exists(id)")
                    .update_expression("SET last_used_at = :last_used_at, passkey = :passkey_json")
                    .expression_attribute_values(
                        ":last_used_at",
                        AttributeValue::N(now.to_string()),
                    )
                    .expression_attribute_values(":passkey_json", AttributeValue::S(passkey_json))
                    .return_consumed_capacity(ReturnConsumedCapacity::Total)
                    .send()
                    .await
                    .map_err(|e| map_update_err(e, format!("WebauthnCredential {id}")))?;
                record_capacity(
                    "update_webauthn_credential_touch",
                    resp.consumed_capacity(),
                    CapKind::Write,
                );
            }
        }
        Ok(())
    }

    async fn delete_webauthn_credential(&self, id: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("webauthn_credential"))
            .key("id", AttributeValue::S(id.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    // ── ephemeral_state (generic) ────────────────────────────────────────────

    async fn put_ephemeral_state(
        &self,
        id: &str,
        kind: &str,
        payload: &str,
        expires_at: u64,
    ) -> db::Result<()> {
        self.ensure_writable()?;
        let resp = self
            .client
            .put_item()
            .table_name(self.table_name("ephemeral_state"))
            .item("id", AttributeValue::S(id.to_string()))
            .item("kind", AttributeValue::S(kind.to_string()))
            .item("payload", AttributeValue::S(payload.to_string()))
            .item("expires_at", AttributeValue::N(expires_at.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "put_ephemeral_state",
            resp.consumed_capacity(),
            CapKind::Write,
        );
        Ok(())
    }

    async fn get_ephemeral_state(&self, id: &str) -> db::Result<Option<db::EphemeralState>> {
        let resp = self
            .client
            .get_item()
            .table_name(self.table_name("ephemeral_state"))
            .key("id", AttributeValue::S(id.to_string()))
            .return_consumed_capacity(ReturnConsumedCapacity::Total)
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        record_capacity(
            "get_ephemeral_state",
            resp.consumed_capacity(),
            CapKind::Read,
        );
        match resp.item {
            Some(item) => Ok(Some(hydrate_item(item)?)),
            None => Ok(None),
        }
    }

    async fn delete_ephemeral_state(&self, id: &str) -> db::Result<()> {
        self.ensure_writable()?;
        self.client
            .delete_item()
            .table_name(self.table_name("ephemeral_state"))
            .key("id", AttributeValue::S(id.to_string()))
            .send()
            .await
            .map_err(|e| db::Error::Infrastructure(sdk_err_msg(e)))?;
        Ok(())
    }

    // ── WebAuthn challenge state — a thin view over ephemeral_state ─────────

    async fn put_webauthn_state(
        &self,
        id: &str,
        kind: &str,
        user_id: Option<&str>,
        state_json: &str,
        expires_at: u64,
    ) -> db::Result<()> {
        let payload = serde_json::json!({
            "user_id": user_id,
            "state_json": state_json,
        })
        .to_string();
        self.put_ephemeral_state(id, kind, &payload, expires_at)
            .await
    }

    async fn get_webauthn_state(&self, id: &str) -> db::Result<Option<db::WebauthnState>> {
        let Some(state) = self.get_ephemeral_state(id).await? else {
            return Ok(None);
        };
        let payload: serde_json::Value = serde_json::from_str(&state.payload)
            .map_err(|e| db::Error::Hydration(format!("WebauthnState payload: {e}")))?;
        let state_json = payload
            .get("state_json")
            .and_then(|v| v.as_str())
            .ok_or_else(|| db::Error::Hydration("WebauthnState payload missing state_json".into()))?
            .to_string();
        let user_id = payload
            .get("user_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(Some(db::WebauthnState {
            id: state.id,
            kind: state.kind,
            user_id,
            state_json,
            expires_at: state.expires_at,
        }))
    }

    async fn delete_webauthn_state(&self, id: &str) -> db::Result<()> {
        self.delete_ephemeral_state(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(fields: Vec<(&str, AttributeValue)>) -> Item {
        Item(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn new_id_is_a_12_char_nanoid() {
        let id = new_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn id_reads_the_id_field() {
        let i = item(vec![("id", AttributeValue::S("abc123".into()))]);
        assert_eq!(i.id().unwrap(), "abc123");
    }

    #[test]
    fn id_errors_when_missing() {
        let i = item(vec![]);
        assert!(i.id().is_err());
    }

    #[test]
    fn id_errors_when_not_a_string() {
        let i = item(vec![("id", AttributeValue::N("1".into()))]);
        assert!(i.id().is_err());
    }

    #[test]
    fn string_field_reads_present_and_missing() {
        let i = item(vec![("name", AttributeValue::S("hi".into()))]);
        assert_eq!(i.string_field("name").unwrap(), Some("hi".to_string()));
        assert_eq!(i.string_field("missing").unwrap(), None);
    }

    #[test]
    fn string_field_errors_on_wrong_type() {
        let i = item(vec![("name", AttributeValue::N("1".into()))]);
        assert!(i.string_field("name").is_err());
    }

    #[test]
    fn i64_field_reads_present_and_missing() {
        let i = item(vec![("n", AttributeValue::N("42".into()))]);
        assert_eq!(i.i64_field("n").unwrap(), Some(42));
        assert_eq!(i.i64_field("missing").unwrap(), None);
    }

    #[test]
    fn bool_field_reads_present_and_missing() {
        let i = item(vec![("b", AttributeValue::Bool(true))]);
        assert_eq!(i.bool_field("b").unwrap(), Some(true));
        assert_eq!(i.bool_field("missing").unwrap(), None);
    }

    #[test]
    fn string_set_field_missing_is_empty() {
        let i = item(vec![]);
        assert_eq!(i.string_set_field("tags").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn string_set_field_reads_the_set() {
        let i = item(vec![(
            "tags",
            AttributeValue::Ss(vec!["a".into(), "b".into()]),
        )]);
        assert_eq!(i.string_set_field("tags").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn has_field_is_true_regardless_of_type() {
        let i = item(vec![("marker", AttributeValue::N("1".into()))]);
        assert!(i.has_field("marker"));
        assert!(!i.has_field("absent"));
    }

    /// A tiny hydration target, local to the tests, exercising [`hydrate_item`] and
    /// [`hydrate_items_lenient`]'s per-row id tagging.
    #[derive(Debug, PartialEq)]
    struct TestRow {
        id: String,
        name: String,
    }

    impl HasID for TestRow {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl TryInto<TestRow> for Item {
        type Error = HydrationError;
        fn try_into(self) -> Result<TestRow, Self::Error> {
            Ok(TestRow {
                id: self.id()?,
                name: self
                    .string_field("name")?
                    .ok_or_else(|| anyhow!("TestRow missing name"))?,
            })
        }
    }

    #[test]
    fn hydrate_items_lenient_reports_each_row_independently() {
        let rows = vec![
            HashMap::from([
                ("id".to_string(), AttributeValue::S("r1".into())),
                ("name".to_string(), AttributeValue::S("Row One".into())),
            ]),
            // Missing `name` — should fail, tagged with its id.
            HashMap::from([("id".to_string(), AttributeValue::S("r2".into()))]),
        ];
        let results: Vec<HydrationResult<TestRow>> = hydrate_items_lenient(Some(rows));
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].as_ref().unwrap(),
            &TestRow {
                id: "r1".into(),
                name: "Row One".into()
            }
        );
        let err = results[1].as_ref().unwrap_err();
        assert_eq!(err.record_id(), Some("r2"));
    }

    #[test]
    fn hydrate_items_lenient_of_empty_input_is_empty() {
        let results: Vec<HydrationResult<TestRow>> = hydrate_items_lenient(None);
        assert!(results.is_empty());
    }

    #[test]
    fn page_scan_direction_no_cursor_scans_in_the_list_order() {
        // descending=true (the default, newest-first list): no cursor scans
        // backward through the index (scan_forward=false) to get the newest
        // items first, and there's no "previous page" boundary to speak of.
        assert_eq!(page_scan_direction(false, false, true), (false, false));
        assert_eq!(page_scan_direction(false, false, false), (true, false));
    }

    #[test]
    fn page_scan_direction_after_cursor_continues_forward_in_list_order() {
        assert_eq!(page_scan_direction(true, false, true), (false, false));
        assert_eq!(page_scan_direction(true, false, false), (true, false));
    }

    #[test]
    fn page_scan_direction_before_cursor_scans_backward_and_flags_reverse() {
        // `before` means "give me the page ending just before this cursor" —
        // scanned against the list's order, then reversed back for output.
        assert_eq!(page_scan_direction(false, true, true), (true, true));
        assert_eq!(page_scan_direction(false, true, false), (false, true));
    }
}
