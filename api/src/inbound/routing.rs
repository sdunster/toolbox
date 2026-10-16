//! Recipient → instance resolution for inbound mail.
//!
//! This module is pure by construction — no I/O — so the resolution *order* is
//! exhaustively unit-tested without a database. [`resolve_instance_id`] at the
//! bottom is the one function that does I/O: it's the "db helper" the caller
//! (the worker Lambda's inbound-mail path, and this step's CLI/seed-fixture tests) uses
//! to turn a real DynamoDB read into an answer, wired to the same ordering
//! logic [`resolve`] encodes.
//!
//! Resolution order per the build plan: for each candidate recipient
//! (typically To + Cc), lowercase, strip a `+tag` suffix from the local part,
//! then try an exact `inbound_address` match on the stripped address, then a
//! `*@domain` wildcard match. First hit — across all recipients, exact before
//! wildcard within each — wins; no hit is a miss (the caller drops the
//! message).

use crate::db;

/// One recipient address, normalized for inbound-address lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedRecipient {
    /// Lowercased, with any `+tag` suffix stripped from the local part —
    /// exactly the form `inbound_address.address` is stored in for an exact
    /// match.
    pub address: String,
    /// The `+tag` suffix, if the local part had one (without the leading
    /// `+`). Carries the ticket reply token in step 7; unused by resolution
    /// itself.
    pub tag: Option<String>,
    /// The domain part, if the address contained an `@` at all. `None` for a
    /// malformed address (no `@`), which also means no wildcard fallback is
    /// possible for it.
    pub domain: Option<String>,
}

/// Lowercase a raw recipient address for matching, and split a `+tag`
/// suffix off the local part, if present. A `+` only counts within the
/// local part (before the first `@`) — a bare address with no `@` has no
/// local/domain split to speak of, so it's returned as-is (lowercased) with
/// no tag and no domain.
///
/// Only the *first* `+` in the local part is treated as the tag delimiter:
/// `user+tag+extra@example.com` strips to local `user`, tag `tag+extra` — the
/// tag itself may contain further `+` characters (step 7 shapes it as
/// `t{ticket_id}.{reply_token}`, but nothing here assumes that).
///
/// **The tag's own case is preserved, unlike `address`/`domain`.** Every id
/// this project mints (`dynamodb::new_id`, and the reply token alongside it)
/// is a nanoid drawn from a *mixed-case* alphabet — lowercasing the tag
/// before `inbound::resolution::parse_reply_tag` splits it would corrupt the
/// embedded `ticket_id` for the (overwhelming majority of) tickets whose id
/// contains an uppercase character, turning a strongly-consistent `GetItem`
/// by primary key into a silent `NotFound`. Address matching against
/// `inbound_address` stays case-insensitive (email domains are
/// case-insensitive by spec, and this project always stores addresses
/// lowercased), so only the tag half of the split needs this care.
pub fn normalize_recipient(raw: &str) -> NormalizedRecipient {
    match raw.split_once('@') {
        Some((local, domain)) => {
            let domain_lower = domain.to_lowercase();
            match local.split_once('+') {
                Some((base, tag)) => NormalizedRecipient {
                    address: format!("{}@{domain_lower}", base.to_lowercase()),
                    tag: Some(tag.to_string()),
                    domain: Some(domain_lower),
                },
                None => NormalizedRecipient {
                    address: format!("{}@{domain_lower}", local.to_lowercase()),
                    tag: None,
                    domain: Some(domain_lower),
                },
            }
        }
        None => NormalizedRecipient {
            address: raw.to_lowercase(),
            tag: None,
            domain: None,
        },
    }
}

/// The `*@domain` wildcard key for a domain, as stored in `inbound_address`.
pub fn wildcard_for_domain(domain: &str) -> String {
    format!("*@{domain}")
}

/// The `inbound_address` lookup keys to try for one raw recipient, in order:
/// the normalized exact address, then (if the address had a domain at all)
/// its domain's wildcard key.
pub fn lookup_keys(raw: &str) -> Vec<String> {
    let n = normalize_recipient(raw);
    let mut keys = vec![n.address];
    if let Some(domain) = n.domain {
        keys.push(wildcard_for_domain(&domain));
    }
    keys
}

/// Resolve the first of several candidate recipients that matches a known
/// inbound address, and return the matched lookup key (an exact address or a
/// `*@domain` wildcard) — not an instance id, since this function has no
/// concept of one. Recipients are tried in order; within a recipient, the
/// exact address is tried before its domain's wildcard. First hit wins.
///
/// `is_known` stands in for a database lookup ("does this key exist"), kept
/// as an injected closure so this stays pure and exhaustively unit-testable
/// with a fake in-memory set. [`resolve_instance_id`] is the impure caller
/// that wires this ordering to a real `db::Handler`.
pub fn resolve<F: FnMut(&str) -> bool>(recipients: &[&str], mut is_known: F) -> Option<String> {
    for recipient in recipients {
        for key in lookup_keys(recipient) {
            if is_known(&key) {
                return Some(key);
            }
        }
    }
    None
}

/// Normalize and classify a raw address for storage as an `inbound_address`
/// row: lowercase it, and detect the `*@domain` wildcard form (a literal `*`
/// local part). Used by the owner-facing `addInboundAddress` mutation and
/// `bin/cli.rs`'s `address add`, so both validate the same way. Rejects
/// anything that isn't a plausible address or wildcard: no `@`, an empty
/// local or domain part, or a `*` anywhere other than as the entire local
/// part of a wildcard.
pub fn classify_for_storage(raw: &str) -> Result<(String, db::AddressKind), String> {
    let lower = raw.trim().to_lowercase();
    let Some((local, domain)) = lower.split_once('@') else {
        return Err(format!("{raw:?} is not a valid address (missing '@')"));
    };
    if domain.is_empty() {
        return Err(format!("{raw:?} has no domain"));
    }
    if local == "*" {
        return Ok((lower, db::AddressKind::Wildcard));
    }
    if local.contains('*') || domain.contains('*') {
        return Err(format!(
            "{raw:?}: '*' is only allowed as the entire local part of a wildcard (*@domain)"
        ));
    }
    if local.is_empty() {
        return Err(format!("{raw:?} has no local part"));
    }
    Ok((lower, db::AddressKind::Exact))
}

/// Resolve the instance a batch of candidate recipients (typically To + Cc)
/// belongs to, against a real database. Uses [`resolve`]'s ordering: for each
/// recipient in turn, the exact normalized address before the domain
/// wildcard; the first recipient with a match wins. Returns `None` — a miss,
/// not an error — when nothing matches; the caller (step 7's inbound-mail
/// Lambda) drops the message in that case.
///
/// This is the one function in this module that performs I/O.
pub async fn resolve_instance_id<H: db::Handler>(
    db: &H,
    recipients: &[impl AsRef<str>],
) -> db::Result<Option<String>> {
    for recipient in recipients {
        for key in lookup_keys(recipient.as_ref()) {
            if let Some(row) = db.get_inbound_address(&key).await? {
                return Ok(Some(row.instance_id));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn normalize_lowercases() {
        let n = normalize_recipient("Support@Example.COM");
        assert_eq!(n.address, "support@example.com");
        assert_eq!(n.tag, None);
        assert_eq!(n.domain.as_deref(), Some("example.com"));
    }

    #[test]
    fn normalize_strips_a_plus_tag() {
        let n = normalize_recipient("support+t123abc@example.com");
        assert_eq!(n.address, "support@example.com");
        assert_eq!(n.tag.as_deref(), Some("t123abc"));
        assert_eq!(n.domain.as_deref(), Some("example.com"));
    }

    #[test]
    fn normalize_keeps_only_the_text_after_the_first_plus_as_the_tag() {
        // Multiple '+' characters: only the first splits local/tag; the rest of
        // the tag is left intact, and stripped from the matched address.
        let n = normalize_recipient("support+t1+t2@example.com");
        assert_eq!(n.address, "support@example.com");
        assert_eq!(n.tag.as_deref(), Some("t1+t2"));
    }

    #[test]
    fn normalize_lowercases_the_address_but_preserves_tag_case() {
        // See `normalize_recipient`'s doc comment: the tag carries a
        // mixed-case nanoid ticket id (step 7's `+t{ticket_id}.{reply_token}`
        // format), so lowercasing it would corrupt the id.
        let n = normalize_recipient("Support+TAG@Example.com");
        assert_eq!(n.address, "support@example.com");
        assert_eq!(n.tag.as_deref(), Some("TAG"));
    }

    #[test]
    fn normalize_preserves_mixed_case_within_a_reply_tag() {
        let n = normalize_recipient("support+tAbC123XyZ987.RepLyTok3n@Example.com");
        assert_eq!(n.address, "support@example.com");
        assert_eq!(n.tag.as_deref(), Some("tAbC123XyZ987.RepLyTok3n"));
    }

    #[test]
    fn normalize_handles_a_malformed_address_with_no_at() {
        let n = normalize_recipient("Not-An-Email");
        assert_eq!(n.address, "not-an-email");
        assert_eq!(n.tag, None);
        assert_eq!(n.domain, None);
    }

    #[test]
    fn lookup_keys_for_a_normal_address_is_exact_then_wildcard() {
        assert_eq!(
            lookup_keys("Support+tag@Example.com"),
            vec!["support@example.com", "*@example.com"]
        );
    }

    #[test]
    fn lookup_keys_for_a_malformed_address_has_no_wildcard_fallback() {
        assert_eq!(lookup_keys("not-an-email"), vec!["not-an-email"]);
    }

    #[test]
    fn resolve_matches_an_exact_address() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(&["support@example.com"], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn resolve_strips_the_tag_before_matching() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(&["support+t123@example.com"], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn resolve_folds_case() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(&["SUPPORT@EXAMPLE.COM"], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn resolve_falls_back_to_a_wildcard() {
        let known: HashSet<&str> = ["*@example.com"].into_iter().collect();
        let hit = resolve(&["anything@example.com"], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("*@example.com"));
    }

    #[test]
    fn resolve_wildcard_does_not_match_a_different_domain() {
        let known: HashSet<&str> = ["*@example.com"].into_iter().collect();
        let hit = resolve(&["anything@example.org"], |k| known.contains(k));
        assert_eq!(hit, None);
    }

    #[test]
    fn resolve_returns_none_for_no_match() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(&["nobody@example.com"], |k| known.contains(k));
        assert_eq!(hit, None);
    }

    #[test]
    fn resolve_tries_every_recipient_and_returns_the_one_that_matches() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(
            &[
                "nobody@example.com",
                "support@example.com",
                "cc@example.org",
            ],
            |k| known.contains(k),
        );
        assert_eq!(hit.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn resolve_prefers_an_earlier_recipient_over_a_later_one() {
        // Both recipients would match (via different known addresses); the
        // first recipient in the list wins.
        let known: HashSet<&str> = ["a@example.com", "b@example.com"].into_iter().collect();
        let hit = resolve(&["b@example.com", "a@example.com"], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("b@example.com"));
    }

    #[test]
    fn resolve_within_one_recipient_prefers_exact_over_wildcard() {
        let known: HashSet<&str> = ["support@example.com", "*@example.com"]
            .into_iter()
            .collect();
        let hit = resolve(&["support@example.com"], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn resolve_handles_a_malformed_address_among_recipients() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(&["not-an-email", "support@example.com"], |k| {
            known.contains(k)
        });
        assert_eq!(hit.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn classify_for_storage_lowercases_an_exact_address() {
        let (addr, kind) = classify_for_storage("Support@Example.COM").unwrap();
        assert_eq!(addr, "support@example.com");
        assert_eq!(kind, db::AddressKind::Exact);
    }

    #[test]
    fn classify_for_storage_recognizes_a_wildcard() {
        let (addr, kind) = classify_for_storage("*@Example.com").unwrap();
        assert_eq!(addr, "*@example.com");
        assert_eq!(kind, db::AddressKind::Wildcard);
    }

    #[test]
    fn classify_for_storage_rejects_no_at() {
        assert!(classify_for_storage("not-an-email").is_err());
    }

    #[test]
    fn classify_for_storage_rejects_empty_domain() {
        assert!(classify_for_storage("support@").is_err());
    }

    #[test]
    fn classify_for_storage_rejects_empty_local() {
        assert!(classify_for_storage("@example.com").is_err());
    }

    #[test]
    fn classify_for_storage_rejects_a_partial_wildcard() {
        assert!(classify_for_storage("supp*ort@example.com").is_err());
        assert!(classify_for_storage("*supp@example.com").is_err());
    }

    #[test]
    fn resolve_a_malformed_address_alone_is_a_miss() {
        let known: HashSet<&str> = ["support@example.com"].into_iter().collect();
        let hit = resolve(&["not-an-email"], |k| known.contains(k));
        assert_eq!(hit, None);
    }
}
