//! Brands — the names one seller invoices under (see CLAUDE.md's "Brands"
//! entry). The instance's own finance settings are the default brand;
//! [`db::Brand`]s on the instance row are the others. A brand changes only
//! what is printed as the seller's name and contact/payment details; ABN,
//! GST registration, currency, payment terms and numbering stay the
//! instance's.

use crate::db;

/// The seller details an invoice prints, after a brand's overrides have
/// been applied over the instance's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveSeller {
    pub name: Option<String>,
    /// The instance's `business_name` (the legal name), only when a
    /// non-default brand applies and its name differs — printed as
    /// "{legal_name} trading as {name}".
    pub legal_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub payment_details: Option<String>,
}

/// The stored brand `brand_id` names on `instance`, if any. `None` for the
/// default brand, and for an id the instance doesn't have.
pub fn find<'a>(instance: &'a db::Instance, brand_id: Option<&str>) -> Option<&'a db::Brand> {
    let id = brand_id?;
    instance.brands.iter().find(|b| b.id == id)
}

/// What an invoice under `brand_id` prints as its seller. A brand id the
/// instance no longer has falls back to the default brand.
pub fn resolve(instance: &db::Instance, brand_id: Option<&str>) -> EffectiveSeller {
    match find(instance, brand_id) {
        None => EffectiveSeller {
            name: instance.business_name.clone(),
            legal_name: None,
            email: instance.business_email.clone(),
            phone: instance.business_phone.clone(),
            address: instance.business_address.clone(),
            payment_details: instance.payment_details.clone(),
        },
        Some(brand) => EffectiveSeller {
            name: Some(brand.name.clone()),
            legal_name: instance
                .business_name
                .clone()
                .filter(|legal| legal != &brand.name),
            email: brand
                .email
                .clone()
                .or_else(|| instance.business_email.clone()),
            phone: brand
                .phone
                .clone()
                .or_else(|| instance.business_phone.clone()),
            address: brand
                .address
                .clone()
                .or_else(|| instance.business_address.clone()),
            payment_details: brand
                .payment_details
                .clone()
                .or_else(|| instance.payment_details.clone()),
        },
    }
}

/// Check that `brand_id` (as given by a caller) can be chosen for a project
/// or expense on `instance`, returning what to store: `None` for the
/// default brand. `current` is the row's existing brand, which stays
/// allowed even once archived, so editing an old row doesn't force a
/// brand change.
pub fn validate_choice(
    instance: &db::Instance,
    brand_id: Option<&str>,
    current: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(id) = brand_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    if id == db::DEFAULT_BRAND_ID {
        return Ok(None);
    }
    match instance.brands.iter().find(|b| b.id == id) {
        None => Err(format!("Brand {id:?} does not exist in this instance")),
        Some(b) if b.archived && current != Some(id) => {
            Err(format!("Brand {:?} is archived", b.name))
        }
        Some(_) => Ok(Some(id.to_string())),
    }
}

/// Validate a `brandId` filter argument against `instance`: the default
/// brand or one of its brands (archived ones included).
pub fn validate_filter<'a>(
    instance: &db::Instance,
    brand_id: &'a str,
) -> Result<db::BrandFilter<'a>, String> {
    let filter = db::BrandFilter::from_id(brand_id);
    if let db::BrandFilter::Brand(id) = filter
        && !instance.brands.iter().any(|b| b.id == id)
    {
        return Err(format!("Brand {id:?} does not exist in this instance"));
    }
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(brands: Vec<db::Brand>) -> db::Instance {
        db::Instance {
            id: "inst".into(),
            name: "Books".into(),
            slug: "books".into(),
            kind: db::InstanceKind::Finance,
            public_submission_enabled: false,
            from_name: String::new(),
            signature: String::new(),
            created_at: 0,
            deleted: false,
            business_name: Some("Samuel James Dunster".into()),
            business_abn: Some("12 345 678 901".into()),
            business_address: Some("1 Legal St".into()),
            business_phone: Some("0400 000 000".into()),
            business_email: Some("sam@example.com".into()),
            payment_details: Some("BSB 000-000".into()),
            gst_registered: true,
            currency: None,
            payment_terms_days: None,
            broadcast_senders: Vec::new(),
            brands,
            number_format: db::NumberFormat::default(),
        }
    }

    fn brand(id: &str, name: &str) -> db::Brand {
        db::Brand {
            id: id.into(),
            name: name.into(),
            email: None,
            phone: None,
            address: None,
            payment_details: None,
            archived: false,
        }
    }

    #[test]
    fn default_brand_is_the_instance_settings() {
        let i = instance(vec![brand("b1", "Positive Rate")]);
        let s = resolve(&i, None);
        assert_eq!(s.name.as_deref(), Some("Samuel James Dunster"));
        assert_eq!(s.legal_name, None);
        assert_eq!(s.email.as_deref(), Some("sam@example.com"));
        // An unknown id falls back the same way.
        assert_eq!(resolve(&i, Some("gone")), s);
    }

    #[test]
    fn a_brand_overrides_what_it_sets_and_falls_back_for_the_rest() {
        let mut b = brand("b1", "Positive Rate");
        b.email = Some("hello@positiverate.example".into());
        let i = instance(vec![b]);
        let s = resolve(&i, Some("b1"));
        assert_eq!(s.name.as_deref(), Some("Positive Rate"));
        assert_eq!(s.legal_name.as_deref(), Some("Samuel James Dunster"));
        assert_eq!(s.email.as_deref(), Some("hello@positiverate.example"));
        assert_eq!(s.phone.as_deref(), Some("0400 000 000"));
        assert_eq!(s.address.as_deref(), Some("1 Legal St"));
        assert_eq!(s.payment_details.as_deref(), Some("BSB 000-000"));
    }

    #[test]
    fn no_legal_name_line_when_the_brand_is_the_legal_name() {
        let i = instance(vec![brand("b1", "Samuel James Dunster")]);
        assert_eq!(resolve(&i, Some("b1")).legal_name, None);
    }

    #[test]
    fn validate_choice_rules() {
        let mut archived = brand("old", "Old Name");
        archived.archived = true;
        let i = instance(vec![brand("b1", "Positive Rate"), archived]);
        assert_eq!(validate_choice(&i, None, None), Ok(None));
        assert_eq!(validate_choice(&i, Some(""), None), Ok(None));
        assert_eq!(validate_choice(&i, Some("default"), Some("b1")), Ok(None));
        assert_eq!(validate_choice(&i, Some("b1"), None), Ok(Some("b1".into())));
        assert!(validate_choice(&i, Some("nope"), None).is_err());
        assert!(validate_choice(&i, Some("old"), None).is_err());
        assert_eq!(
            validate_choice(&i, Some("old"), Some("old")),
            Ok(Some("old".into()))
        );
    }

    #[test]
    fn validate_filter_rules() {
        let i = instance(vec![brand("b1", "Positive Rate")]);
        assert_eq!(validate_filter(&i, "default"), Ok(db::BrandFilter::Default));
        assert_eq!(validate_filter(&i, "b1"), Ok(db::BrandFilter::Brand("b1")));
        assert!(validate_filter(&i, "nope").is_err());
    }

    #[test]
    fn brand_filter_matches_rows() {
        assert!(db::BrandFilter::Default.matches(None));
        assert!(!db::BrandFilter::Default.matches(Some("b1")));
        assert!(db::BrandFilter::Brand("b1").matches(Some("b1")));
        assert!(!db::BrandFilter::Brand("b1").matches(None));
    }
}
