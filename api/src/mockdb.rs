//! In-process implementation of [`crate::db::Handler`] that fails every method.
//!
//! This is *not* a lightweight substitute for a real database — its job is
//! exercising error paths (what happens when the DB call in the middle of a
//! resolver fails?), not standing in for one. Tests that need actual data need a
//! real DynamoDB (Local or otherwise); `bin/export-schema` and
//! `tests/graphql_error_codes.rs` use this because they only need *a* type that
//! implements [`crate::db::Handler`], never because they read or write through it.

use crate::db;

#[derive(Debug, Default, Clone, Copy)]
pub struct Handler;

impl Handler {
    pub fn new() -> Self {
        Self
    }

    /// Every method routes through this rather than inventing its own error, so a
    /// mockdb failure always reads the same way regardless of which method
    /// produced it.
    fn unsupported<T>() -> db::Result<T> {
        Err(db::Error::Infrastructure(
            "mockdb operation not implemented".to_string(),
        ))
    }
}

impl db::Handler for Handler {
    // ── instance ──────────────────────────────────────────────────────────

    async fn get_instances<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::Instance>>> {
        Self::unsupported()
    }

    async fn get_instance_id_by_slug(&self, _slug: &str) -> db::Result<Option<String>> {
        Self::unsupported()
    }

    async fn create_instance(
        &self,
        _name: &str,
        _slug: &str,
        _from_name: &str,
        _signature: &str,
        _public_submission_enabled: bool,
        _kind: db::InstanceKind,
    ) -> db::Result<db::Instance> {
        Self::unsupported()
    }

    async fn update_instance(
        &self,
        _id: &str,
        _change: db::InstanceUpdateShape<'_>,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn list_instances(&self) -> db::Result<Vec<db::Instance>> {
        Self::unsupported()
    }

    // ── inbound_address ──────────────────────────────────────────────────────

    async fn create_inbound_address(
        &self,
        _address: &str,
        _instance_id: &str,
        _kind: db::AddressKind,
    ) -> db::Result<db::InboundAddress> {
        Self::unsupported()
    }

    async fn get_inbound_address(&self, _address: &str) -> db::Result<Option<db::InboundAddress>> {
        Self::unsupported()
    }

    async fn delete_inbound_address(&self, _address: &str) -> db::Result<()> {
        Self::unsupported()
    }

    async fn list_inbound_addresses_by_instance(
        &self,
        _instance_id: &str,
    ) -> db::Result<Vec<db::InboundAddress>> {
        Self::unsupported()
    }

    // ── membership ────────────────────────────────────────────────────────

    async fn create_membership(
        &self,
        _user_id: &str,
        _instance_id: &str,
        _role: db::MembershipRole,
    ) -> db::Result<db::Membership> {
        Self::unsupported()
    }

    async fn delete_membership(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }

    async fn update_membership_role(&self, _id: &str, _role: db::MembershipRole) -> db::Result<()> {
        Self::unsupported()
    }

    async fn update_membership_notification_settings(
        &self,
        _id: &str,
        _patch: &db::NotificationSettingsPatch,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn list_memberships_by_user(&self, _user_id: &str) -> db::Result<Vec<db::Membership>> {
        Self::unsupported()
    }

    async fn list_memberships_by_instance(
        &self,
        _instance_id: &str,
    ) -> db::Result<Vec<db::Membership>> {
        Self::unsupported()
    }

    // ── user ──────────────────────────────────────────────────────────────

    async fn get_users<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::User>>> {
        Self::unsupported()
    }

    async fn get_user_id_by_email(&self, _email: &str) -> db::Result<Option<String>> {
        Self::unsupported()
    }

    async fn create_user(&self, _email: &str, _name: &str) -> db::Result<db::User> {
        Self::unsupported()
    }

    async fn list_users(&self) -> db::Result<Vec<db::User>> {
        Self::unsupported()
    }

    async fn update_user(&self, _id: &str, _change: db::UserUpdateShape<'_>) -> db::Result<()> {
        Self::unsupported()
    }

    // ── login_code ────────────────────────────────────────────────────────

    async fn put_login_code(
        &self,
        _email: &str,
        _code_hash: &str,
        _expires_at: u64,
        _now: u64,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn get_login_code(&self, _email: &str) -> db::Result<Option<db::LoginCode>> {
        Self::unsupported()
    }

    async fn delete_login_code(&self, _email: &str) -> db::Result<()> {
        Self::unsupported()
    }

    async fn increment_login_code_attempts(&self, _email: &str) -> db::Result<()> {
        Self::unsupported()
    }

    // ── user_token ────────────────────────────────────────────────────────

    async fn create_user_token(
        &self,
        _id: &str,
        _token_hash: &str,
        _user_id: &str,
        _expires_at: u64,
    ) -> db::Result<db::UserToken> {
        Self::unsupported()
    }

    async fn get_user_token(&self, _id: &str) -> db::Result<Option<db::UserToken>> {
        Self::unsupported()
    }

    async fn update_user_token(
        &self,
        _id: &str,
        _change: db::UserTokenUpdateShape,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn delete_user_token(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }

    // ── oauth_grant ───────────────────────────────────────────────────────

    async fn create_oauth_grant(&self, _grant: &db::OAuthGrant) -> db::Result<()> {
        Self::unsupported()
    }

    async fn get_oauth_grant(&self, _id: &str) -> db::Result<Option<db::OAuthGrant>> {
        Self::unsupported()
    }

    async fn update_oauth_grant(
        &self,
        _id: &str,
        _change: db::OAuthGrantUpdateShape,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn delete_oauth_grant(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }

    async fn list_oauth_grants_by_user(&self, _user_id: &str) -> db::Result<Vec<db::OAuthGrant>> {
        Self::unsupported()
    }

    // ── api_token ─────────────────────────────────────────────────────────

    async fn create_api_token(
        &self,
        _id: &str,
        _instance_id: &str,
        _name: &str,
        _token_hash: &str,
        _created_by_user_id: &str,
    ) -> db::Result<db::ApiToken> {
        Self::unsupported()
    }

    async fn get_api_token(&self, _id: &str) -> db::Result<Option<db::ApiToken>> {
        Self::unsupported()
    }

    async fn list_api_tokens_by_instance(
        &self,
        _instance_id: &str,
    ) -> db::Result<Vec<db::ApiToken>> {
        Self::unsupported()
    }

    async fn update_api_token(
        &self,
        _id: &str,
        _change: db::ApiTokenUpdateShape<'_>,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn delete_api_token(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }

    // ── project ───────────────────────────────────────────────────────────

    async fn create_project(
        &self,
        _instance_id: &str,
        _fields: &db::ProjectFields,
    ) -> db::Result<db::Project> {
        Self::unsupported()
    }

    async fn get_projects<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::Project>>> {
        Self::unsupported()
    }

    async fn list_projects_by_instance(&self, _instance_id: &str) -> db::Result<Vec<db::Project>> {
        Self::unsupported()
    }

    async fn update_project(
        &self,
        _id: &str,
        _change: db::ProjectUpdateShape<'_>,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    // ── billable_item ─────────────────────────────────────────────────────

    async fn create_billable_item(
        &self,
        _item: &db::NewBillableItem<'_>,
    ) -> db::Result<db::BillableItem> {
        Self::unsupported()
    }

    async fn rebill_expense(
        &self,
        _expense_id: &str,
        _item: &db::NewBillableItem<'_>,
    ) -> db::Result<Option<db::BillableItem>> {
        Self::unsupported()
    }

    async fn get_billable_items<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::BillableItem>>> {
        Self::unsupported()
    }

    async fn update_billable_item(
        &self,
        _id: &str,
        _invoice_id: Option<&str>,
        _change: db::BillableItemUpdateShape<'_>,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn delete_billable_item(
        &self,
        _id: &str,
        _source_expense_id: Option<&str>,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn list_billable_items(
        &self,
        _scope: db::BillableItemScope<'_>,
        _filter: db::BillableItemFilter,
        _page: db::ListBillableItemsPage,
    ) -> db::Result<Vec<db::BillableItem>> {
        Self::unsupported()
    }

    async fn get_billable_items_consistent<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::BillableItem>>> {
        Self::unsupported()
    }

    // ── expense ───────────────────────────────────────────────────────────

    async fn create_expense(
        &self,
        _instance_id: &str,
        _fields: &db::ExpenseFields,
        _created_by_user_id: &str,
    ) -> db::Result<db::Expense> {
        Self::unsupported()
    }

    async fn get_expenses<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::Expense>>> {
        Self::unsupported()
    }

    async fn update_expense(&self, _id: &str, _fields: &db::ExpenseFields) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn delete_expense(&self, _id: &str) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn set_expense_receipt(
        &self,
        _id: &str,
        _receipt: Option<&db::ExpenseReceipt>,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn list_expenses_in_range(
        &self,
        _instance_id: &str,
        _from: &str,
        _to: &str,
    ) -> db::Result<Vec<db::Expense>> {
        Self::unsupported()
    }

    async fn list_all_expenses_by_project(
        &self,
        _project_id: &str,
    ) -> db::Result<Vec<db::Expense>> {
        Self::unsupported()
    }

    async fn list_expenses(
        &self,
        _scope: db::ExpenseScope<'_>,
        _category: Option<db::ExpenseCategory>,
        _page: db::ListExpensesPage,
    ) -> db::Result<Vec<db::Expense>> {
        Self::unsupported()
    }

    async fn sum_vehicle_km_tenths(
        &self,
        _instance_id: &str,
        _user_id: &str,
        _from: &str,
        _to: &str,
    ) -> db::Result<i64> {
        Self::unsupported()
    }

    // ── invoice ───────────────────────────────────────────────────────────

    async fn get_invoices<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::Invoice>>> {
        Self::unsupported()
    }

    async fn get_invoice_consistent(&self, _id: &str) -> db::Result<Option<db::Invoice>> {
        Self::unsupported()
    }

    async fn create_invoice(
        &self,
        _instance_id: &str,
        _project_id: &str,
        _item_ids: &[String],
        _created_by_user_id: &str,
    ) -> db::Result<Option<db::Invoice>> {
        Self::unsupported()
    }

    async fn add_invoice_items(
        &self,
        _invoice_id: &str,
        _project_id: &str,
        _item_ids: &[String],
        _expected_version: u64,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn remove_invoice_items(
        &self,
        _invoice_id: &str,
        _item_ids: &[String],
        _expected_version: u64,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn delete_invoice(
        &self,
        _invoice_id: &str,
        _item_ids: &[String],
        _expected_version: u64,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn finalize_invoice(
        &self,
        _instance_id: &str,
        _invoice_id: &str,
        _expected_version: u64,
        _finalized: &db::FinalizeInvoice<'_>,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn set_invoice_payments(
        &self,
        _invoice_id: &str,
        _expected_version: u64,
        _payments: &[db::InvoicePayment],
        _paid_date: Option<&str>,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn set_invoice_sent(
        &self,
        _invoice_id: &str,
        _sent_at: u64,
        _to: &[String],
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn list_all_finalized_invoices(
        &self,
        _scope: db::InvoiceScope<'_>,
    ) -> db::Result<Vec<db::Invoice>> {
        Self::unsupported()
    }

    async fn set_invoice_pdf_key(&self, _invoice_id: &str, _key: &str) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn increment_invoice_counter(&self, _instance_id: &str) -> db::Result<u64> {
        Self::unsupported()
    }

    async fn get_invoice_counter(&self, _instance_id: &str) -> db::Result<u64> {
        Self::unsupported()
    }

    async fn set_next_invoice_number(
        &self,
        _instance_id: &str,
        _new_value: u64,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn invoice_number_used(&self, _instance_id: &str, _number: u32) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn list_invoices(
        &self,
        _scope: db::InvoiceScope<'_>,
        _filter: db::InvoiceListFilter,
        _page: db::ListInvoicesPage,
    ) -> db::Result<Vec<db::Invoice>> {
        Self::unsupported()
    }

    // ── credit_note ───────────────────────────────────────────────────────

    async fn create_credit_note(
        &self,
        _note: &db::CreditNote,
        _invoice_expected_version: u64,
        _settled_date: Option<&str>,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn get_credit_notes<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::CreditNote>>> {
        Self::unsupported()
    }

    async fn get_credit_note_consistent(&self, _id: &str) -> db::Result<Option<db::CreditNote>> {
        Self::unsupported()
    }

    async fn list_credit_notes(
        &self,
        _scope: db::CreditNoteScope<'_>,
        _page: db::ListCreditNotesPage,
    ) -> db::Result<Vec<db::CreditNote>> {
        Self::unsupported()
    }

    async fn list_all_credit_notes(
        &self,
        _scope: db::CreditNoteScope<'_>,
    ) -> db::Result<Vec<db::CreditNote>> {
        Self::unsupported()
    }

    async fn increment_credit_note_counter(&self, _instance_id: &str) -> db::Result<u64> {
        Self::unsupported()
    }

    async fn set_credit_note_pdf_key(&self, _id: &str, _key: &str) -> db::Result<bool> {
        Self::unsupported()
    }

    async fn set_credit_note_sent(
        &self,
        _id: &str,
        _sent_at: u64,
        _to: &[String],
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    // ── ticket ────────────────────────────────────────────────────────────

    async fn get_tickets<T: AsRef<str> + Sync>(
        &self,
        _ids: &[T],
    ) -> db::Result<Vec<Option<db::Ticket>>> {
        Self::unsupported()
    }

    async fn increment_ticket_counter(&self, _instance_id: &str) -> db::Result<u64> {
        Self::unsupported()
    }

    async fn create_ticket(
        &self,
        _instance_id: &str,
        _number: u64,
        _subject: &str,
        _requester_emails: &[String],
        _cc_emails: &[String],
    ) -> db::Result<db::Ticket> {
        Self::unsupported()
    }

    async fn update_ticket(&self, _id: &str, _change: db::TicketUpdateShape<'_>) -> db::Result<()> {
        Self::unsupported()
    }

    async fn list_tickets(
        &self,
        _instance_id: &str,
        _filter: db::TicketListFilter,
        _page: db::ListTicketsPage,
    ) -> db::Result<Vec<db::Ticket>> {
        Self::unsupported()
    }

    async fn get_ticket_id_by_instance_number(
        &self,
        _instance_id: &str,
        _number: u64,
    ) -> db::Result<Option<String>> {
        Self::unsupported()
    }

    // ── ticket_message ───────────────────────────────────────────────────

    async fn create_ticket_message(
        &self,
        _ticket_id: &str,
        _kind: db::TicketMessageKind,
        _author_user_id: Option<&str>,
        _from_email: Option<&str>,
        _to_emails: &[String],
        _cc_emails: &[String],
        _body_text: Option<&str>,
        _body_html: Option<&str>,
        _in_reply_to: Option<&str>,
        _references: Option<&str>,
    ) -> db::Result<db::TicketMessage> {
        Self::unsupported()
    }

    async fn list_ticket_messages(&self, _ticket_id: &str) -> db::Result<Vec<db::TicketMessage>> {
        Self::unsupported()
    }

    async fn update_ticket_message(
        &self,
        _id: &str,
        _change: db::TicketMessageUpdateShape<'_>,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn get_ticket_id_by_rfc_message_id(
        &self,
        _rfc_message_id: &str,
    ) -> db::Result<Option<String>> {
        Self::unsupported()
    }

    // ── processed_message ─────────────────────────────────────────────────

    async fn claim_processed_message(
        &self,
        _ses_message_id: &str,
        _now: u64,
        _expires_at: u64,
    ) -> db::Result<bool> {
        Self::unsupported()
    }

    // ── webauthn_credential ───────────────────────────────────────────────

    async fn create_webauthn_credential(
        &self,
        _id: &str,
        _user_id: &str,
        _name: &str,
        _passkey_json: &str,
    ) -> db::Result<db::WebauthnCredential> {
        Self::unsupported()
    }

    async fn get_webauthn_credential(
        &self,
        _id: &str,
    ) -> db::Result<Option<db::WebauthnCredential>> {
        Self::unsupported()
    }

    async fn list_webauthn_credentials_by_user(
        &self,
        _user_id: &str,
    ) -> db::Result<Vec<db::WebauthnCredential>> {
        Self::unsupported()
    }

    async fn count_webauthn_credentials_by_user(&self, _user_id: &str) -> db::Result<usize> {
        Self::unsupported()
    }

    async fn update_webauthn_credential(
        &self,
        _id: &str,
        _change: db::WebauthnCredentialUpdate,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn delete_webauthn_credential(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }

    // ── ephemeral_state (generic) ────────────────────────────────────────────

    async fn put_ephemeral_state(
        &self,
        _id: &str,
        _kind: &str,
        _payload: &str,
        _expires_at: u64,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn get_ephemeral_state(&self, _id: &str) -> db::Result<Option<db::EphemeralState>> {
        Self::unsupported()
    }

    async fn delete_ephemeral_state(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }

    // ── WebAuthn challenge state ──────────────────────────────────────────────

    async fn put_webauthn_state(
        &self,
        _id: &str,
        _kind: &str,
        _user_id: Option<&str>,
        _state_json: &str,
        _expires_at: u64,
    ) -> db::Result<()> {
        Self::unsupported()
    }

    async fn get_webauthn_state(&self, _id: &str) -> db::Result<Option<db::WebauthnState>> {
        Self::unsupported()
    }

    async fn delete_webauthn_state(&self, _id: &str) -> db::Result<()> {
        Self::unsupported()
    }
}
