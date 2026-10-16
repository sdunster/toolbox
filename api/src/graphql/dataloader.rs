//! Per-request batching [`DataLoader`](async_graphql::dataloader::DataLoader) shell.
//!
//! Step 5 adds the first two `impl Loader<XId> for DatabaseLoader<A>` blocks —
//! `UserId`/`InstanceId`, needed because a page of tickets resolves
//! `Ticket.assignee`/`Ticket.instance` once per node. Follows seslogin's
//! `graphql/dataloader.rs` shape exactly: a small `XId(pub ID)` newtype per
//! entity (defined in `graphql::mod`), and a `load` that calls
//! `self.app.db().get_xs(...)` and zips the results back onto the requested
//! keys. [`get_dataloader`](super::get_dataloader) wires one of these into the
//! request context per request.

use std::collections::HashMap;
use std::iter::zip;
use std::sync::Arc;

use anyhow::anyhow;
use async_graphql::dataloader::Loader;

use crate::app::{App, HasDb};
use crate::db;
use crate::db::Handler as _;

use super::{InstanceId, InvoiceId, ProjectId, TicketDeliveriesKey, UserId};

pub struct DatabaseLoader<A: App + HasDb + Send + Sync> {
    app: Arc<A>,
}

impl<A: App + HasDb + Send + Sync> DatabaseLoader<A> {
    pub fn new(app: Arc<A>) -> Self {
        DatabaseLoader { app }
    }
}

impl<A: App + HasDb + Send + Sync + 'static> Loader<UserId> for DatabaseLoader<A> {
    type Value = db::User;
    type Error = Arc<anyhow::Error>;

    async fn load(
        &self,
        keys: &[UserId],
    ) -> std::result::Result<HashMap<UserId, db::User>, Arc<anyhow::Error>> {
        let str_keys = keys.iter().map(|k| k.0.as_str()).collect::<Vec<&str>>();
        let recs = self
            .app
            .db()
            .get_users(&str_keys)
            .await
            .map_err(|e| Arc::new(anyhow!("DB error: {:?}", e)))?;
        // A dataloader reports what exists; ids with no row are simply absent
        // from the map, which is how async-graphql signals "not found" to the
        // resolver (`load_one` returns `Ok(None)` for a missing key).
        let map = zip(keys.iter().cloned(), recs)
            .filter_map(|(key, rec)| rec.map(|r| (key, r)))
            .collect();
        Ok(map)
    }
}

impl<A: App + HasDb + Send + Sync + 'static> Loader<InstanceId> for DatabaseLoader<A> {
    type Value = db::Instance;
    type Error = Arc<anyhow::Error>;

    async fn load(
        &self,
        keys: &[InstanceId],
    ) -> std::result::Result<HashMap<InstanceId, db::Instance>, Arc<anyhow::Error>> {
        let str_keys = keys.iter().map(|k| k.0.as_str()).collect::<Vec<&str>>();
        let recs = self
            .app
            .db()
            .get_instances(&str_keys)
            .await
            .map_err(|e| Arc::new(anyhow!("DB error: {:?}", e)))?;
        let map = zip(keys.iter().cloned(), recs)
            .filter_map(|(key, rec)| rec.map(|r| (key, r)))
            .collect();
        Ok(map)
    }
}

impl<A: App + HasDb + Send + Sync + 'static> Loader<ProjectId> for DatabaseLoader<A> {
    type Value = db::Project;
    type Error = Arc<anyhow::Error>;

    async fn load(
        &self,
        keys: &[ProjectId],
    ) -> std::result::Result<HashMap<ProjectId, db::Project>, Arc<anyhow::Error>> {
        let str_keys = keys.iter().map(|k| k.0.as_str()).collect::<Vec<&str>>();
        let recs = self
            .app
            .db()
            .get_projects(&str_keys)
            .await
            .map_err(|e| Arc::new(anyhow!("DB error: {:?}", e)))?;
        let map = zip(keys.iter().cloned(), recs)
            .filter_map(|(key, rec)| rec.map(|r| (key, r)))
            .collect();
        Ok(map)
    }
}

impl<A: App + HasDb + Send + Sync + 'static> Loader<InvoiceId> for DatabaseLoader<A> {
    type Value = db::Invoice;
    type Error = Arc<anyhow::Error>;

    async fn load(
        &self,
        keys: &[InvoiceId],
    ) -> std::result::Result<HashMap<InvoiceId, db::Invoice>, Arc<anyhow::Error>> {
        let str_keys = keys.iter().map(|k| k.0.as_str()).collect::<Vec<&str>>();
        let recs = self
            .app
            .db()
            .get_invoices(&str_keys)
            .await
            .map_err(|e| Arc::new(anyhow!("DB error: {:?}", e)))?;
        let map = zip(keys.iter().cloned(), recs)
            .filter_map(|(key, rec)| rec.map(|r| (key, r)))
            .collect();
        Ok(map)
    }
}

/// Every `message_delivery` row (delivery and engagement) of each requested ticket (one
/// `ticket_id-index` query per ticket — in practice one per request, since a
/// thread view shows one ticket). A ticket with none maps to an empty list.
impl<A: App + HasDb + Send + Sync + 'static> Loader<TicketDeliveriesKey> for DatabaseLoader<A> {
    type Value = Vec<db::MessageDeliveryRow>;
    type Error = Arc<anyhow::Error>;

    async fn load(
        &self,
        keys: &[TicketDeliveriesKey],
    ) -> std::result::Result<
        HashMap<TicketDeliveriesKey, Vec<db::MessageDeliveryRow>>,
        Arc<anyhow::Error>,
    > {
        let mut map = HashMap::new();
        for key in keys {
            let rows = self
                .app
                .db()
                .list_message_deliveries(key.0.as_str())
                .await
                .map_err(|e| Arc::new(anyhow!("DB error: {:?}", e)))?;
            map.insert(key.clone(), rows);
        }
        Ok(map)
    }
}
