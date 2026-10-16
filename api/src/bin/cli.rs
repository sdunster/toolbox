//! Toolbox `cli` — a thin, ergonomic wrapper over the DB API, following
//! seslogin's `bin/cli.rs` conventions (one subcommand tree per object type,
//! writes happen immediately, `--dry-run` reports what *would* change
//! instead). This is how an operator bootstraps the first organisation and
//! its first owner — see `DEVELOPMENT.md`'s "Bootstrapping the first
//! organisation" section for the sequence this drives.
//!
//! Unlike `bin/local-tables.rs`/`bin/local-seed.rs`, this CLI is **not**
//! restricted to a local DynamoDB endpoint — it is the general admin tool,
//! meant to run against a real deployed `DB_PREFIX` too.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use toolbox::db::{
    self, Handler, Instance, InstanceKind, InstanceUpdateShape, MembershipRole, User,
    UserUpdateShape,
};
use toolbox::dynamodb;
use toolbox::inbound::routing;

#[derive(Parser, Debug)]
#[command(about = "Admin/inspection CLI for the Toolbox DB API")]
struct Cli {
    /// DynamoDB table prefix (e.g. "prod", "local"). Falls back to the
    /// DB_PREFIX env var.
    #[arg(long, global = true)]
    db_prefix: Option<String>,

    /// Report what the command would change without writing anything.
    #[arg(long, global = true, default_value_t = false)]
    dry_run: bool,

    #[command(subcommand)]
    object: Object,
}

#[derive(Subcommand, Debug)]
enum Object {
    /// Tenant organisations.
    Instance {
        #[command(subcommand)]
        cmd: InstanceCmd,
    },
    /// Inbound email addresses mapped to an instance.
    Address {
        #[command(subcommand)]
        cmd: AddressCmd,
    },
    /// System users (people who can log in).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Instance memberships — who belongs to which instance, in what role.
    /// Also manageable over GraphQL (`addMember`/`removeMember`/
    /// `setMemberRole`, superuser-only) — this subcommand remains for
    /// operators working outside the web admin UI.
    Member {
        #[command(subcommand)]
        cmd: MemberCmd,
    },
    /// Inbound mail, exercised locally with no AWS account. Backs `make
    /// local-mail FILE=...`.
    Mail {
        #[command(subcommand)]
        cmd: MailCmd,
    },
    /// Addresses Toolbox no longer mails after a hard bounce or a spam
    /// complaint (see `toolbox::suppression`).
    Suppression {
        #[command(subcommand)]
        cmd: SuppressionCmd,
    },
}

#[derive(Subcommand, Debug)]
enum SuppressionCmd {
    /// List every suppressed address.
    List,
    /// Stop suppressing an address, so instance mail is sent to it again.
    /// Only Toolbox's own list: SES keeps an account-level suppression list
    /// of its own, which this does not touch — see the message printed.
    Remove { email: String },
}

#[derive(Subcommand, Debug)]
enum InstanceCmd {
    /// Create an instance. Fails if the slug is already taken (checked, then
    /// written — see `db::Handler::create_instance`'s doc comment for the
    /// narrow race this leaves open, which is fine for an operator-driven,
    /// low-frequency action like this one).
    Create {
        name: String,
        slug: String,
        /// From: display name for outbound mail. Defaults to `name`.
        #[arg(long)]
        from_name: Option<String>,
        /// Appended to outbound replies. Defaults to empty.
        #[arg(long)]
        signature: Option<String>,
        /// Allow anyone to submit a ticket to this instance from the public
        /// web form, after verifying their email with a code. Support
        /// instances only — see `--kind`.
        #[arg(long)]
        public_submission_enabled: bool,
        /// Which of Toolbox's two functions this instance is for.
        /// Immutable after creation — see `db::InstanceKind`'s doc comment.
        #[arg(long, default_value = "support")]
        kind: KindArg,
    },
    /// List every instance (active and soft-deleted).
    List,
    /// Update an existing instance's fields, or soft-delete/restore it.
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        from_name: Option<String>,
        #[arg(long)]
        signature: Option<String>,
        #[arg(long)]
        public_submission_enabled: Option<bool>,
        /// Soft-delete (`true`) or restore (`false`) the instance. Mutually
        /// exclusive with the field flags above — deletion is its own
        /// write, matching `db::InstanceUpdateShape`.
        #[arg(long, conflicts_with_all = ["name", "from_name", "signature", "public_submission_enabled"])]
        deleted: Option<bool>,
    },
}

#[derive(Subcommand, Debug)]
enum AddressCmd {
    /// Map an inbound address (or `*@domain` wildcard) to an instance.
    /// Normalized and classified by `inbound::routing::classify_for_storage`
    /// — the same logic the `addInboundAddress` GraphQL mutation uses.
    Add {
        #[arg(long)]
        instance: String,
        address: String,
    },
    /// Remove an inbound address.
    Remove { address: String },
    /// List every inbound address mapped to an instance.
    List {
        #[arg(long)]
        instance: String,
    },
}

#[derive(Subcommand, Debug)]
enum UserCmd {
    /// Create a user. Does not create any membership — pair with `member add`.
    Create {
        email: String,
        name: String,
        /// Grant superuser immediately (admin + instance settings across
        /// every instance — never ticket access; see `CLAUDE.md`'s
        /// superuser boundary house rule). Equivalent to `create` followed
        /// by `set-superuser <id> true`.
        #[arg(long)]
        superuser: bool,
    },
    /// Grant or revoke superuser. **The only way to grant it** — no GraphQL
    /// mutation can, by design (see `db::User::superuser`'s doc comment).
    /// `<id-or-email>` accepts either, mirroring `member add`'s `--user`.
    SetSuperuser {
        id_or_email: String,
        /// `true`/`false`. Explicit (not a bare flag) since this is a
        /// positional value, not `--superuser` — mirrors `instance update
        /// --deleted <bool>`'s explicitness for the same reason: a bare
        /// flag can only ever mean "set to true", and revoking needs the
        /// same command shape as granting.
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },
    /// List every user.
    List,
}

#[derive(Subcommand, Debug)]
enum MemberCmd {
    /// Grant a user a role in an instance. `--user` accepts either a user id
    /// or an email (resolved via `email-index`), mirroring
    /// `auth::resolve_dev_auth`'s convention. Fails if the user already has a
    /// membership in that instance — remove it first to change the role.
    Add {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value = "agent")]
        role: RoleArg,
    },
    /// Revoke a user's membership in an instance.
    Remove {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        user: String,
    },
    /// List every member of an instance and their role.
    List {
        #[arg(long)]
        instance: String,
    },
}

#[derive(Subcommand, Debug)]
enum MailCmd {
    /// Feed a raw `.eml` file straight into the same parse → route →
    /// resolve → store pipeline the worker Lambda uses
    /// (`inbound::pipeline::process_raw_message`) — no S3, no SQS. Mail
    /// sending is mocked (`mockmail`, logged/written under `MOCK_MAIL_DIR`)
    /// and attachment/raw-MIME storage is a local directory (`mockstorage`,
    /// under `MOCK_STORAGE_DIR`), exactly as `make dev-local` mocks them —
    /// so this needs a local DynamoDB (`make local-up && make
    /// local-tables`) but no AWS account at all.
    ///
    /// The idempotency key (`ses_message_id`) is derived from the file's
    /// own sha256 — running the same fixture twice is therefore a
    /// deliberate way to exercise the duplicate-delivery no-op path, not an
    /// accident to work around. Pass `--fresh` when you want the opposite:
    /// a new id per invocation, so a fixture can be replayed as if it were
    /// a genuinely new delivery.
    Process {
        /// Path to a raw RFC 5322 `.eml` file.
        #[arg(long)]
        file: PathBuf,
        /// Treat this as a brand-new delivery: generate a unique
        /// `ses_message_id` instead of deriving it from the file's content.
        #[arg(long, default_value_t = false)]
        fresh: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum RoleArg {
    Owner,
    Agent,
}

impl From<RoleArg> for MembershipRole {
    fn from(r: RoleArg) -> Self {
        match r {
            RoleArg::Owner => MembershipRole::Owner,
            RoleArg::Agent => MembershipRole::Agent,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum KindArg {
    Support,
    Invoicing,
}

impl From<KindArg> for InstanceKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Support => InstanceKind::Support,
            KindArg::Invoicing => InstanceKind::Invoicing,
        }
    }
}

fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

fn opt_str(s: &str) -> String {
    if s.is_empty() {
        "-".to_string()
    } else {
        s.to_string()
    }
}

fn print_instance(i: &Instance) {
    println!(
        "id: {}\nname: {}\nslug: {}\nkind: {}\npublic_submission_enabled: {}\nfrom_name: {}\nsignature: {}\ncreated_at: {}\ndeleted: {}",
        i.id,
        i.name,
        i.slug,
        i.kind.as_str(),
        bool_str(i.public_submission_enabled),
        opt_str(&i.from_name),
        opt_str(&i.signature),
        i.created_at,
        bool_str(i.deleted),
    );
}

fn print_user(u: &User) {
    println!(
        "id: {}\nemail: {}\nname: {}\nenabled: {}\ncreated_at: {}\naccess_time: {}\nsuperuser: {}",
        u.id,
        u.email,
        u.name,
        bool_str(u.enabled),
        u.created_at,
        u.access_time
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".to_string()),
        bool_str(u.superuser),
    );
}

/// Resolve `--user` to a user id: an email (contains `@`) is normalized
/// (`db::normalize_user_email` — trimmed and lowercased, matching every
/// other entry point that writes or looks up a user email) and looked up
/// via `email-index`; anything else is treated as a literal id (not
/// verified to exist here — the write that follows will fail loudly if it
/// doesn't).
async fn resolve_user_id(db: &impl Handler, id_or_email: &str) -> Result<String> {
    if id_or_email.contains('@') {
        let email = db::normalize_user_email(id_or_email).map_err(|e| anyhow!(e))?;
        db.get_user_id_by_email(&email)
            .await
            .context("looking up user by email")?
            .ok_or_else(|| anyhow!("no user with email {email}"))
    } else {
        Ok(id_or_email.to_string())
    }
}

async fn run_instance(db: &impl Handler, cmd: InstanceCmd, dry_run: bool) -> Result<()> {
    match cmd {
        InstanceCmd::Create {
            name,
            slug,
            from_name,
            signature,
            public_submission_enabled,
            kind,
        } => {
            db::validate_slug(&slug).map_err(|e| anyhow!(e))?;
            if db.get_instance_id_by_slug(&slug).await?.is_some() {
                return Err(anyhow!("slug {slug:?} is already taken"));
            }
            let kind: InstanceKind = kind.into();
            if public_submission_enabled && kind != InstanceKind::Support {
                return Err(anyhow!(
                    "public_submission_enabled only applies to a support instance"
                ));
            }
            let from_name = from_name.unwrap_or_else(|| name.clone());
            let signature = signature.unwrap_or_default();
            if dry_run {
                println!(
                    "[dry-run] would create instance name={name:?} slug={slug:?} kind={} \
                     from_name={from_name:?} public_submission_enabled={public_submission_enabled}",
                    kind.as_str()
                );
                return Ok(());
            }
            let instance = db
                .create_instance(
                    &name,
                    &slug,
                    &from_name,
                    &signature,
                    public_submission_enabled,
                    kind,
                )
                .await?;
            print_instance(&instance);
        }
        InstanceCmd::List => {
            let instances = db.list_instances().await?;
            println!(
                "{:>12}  {:<20}  {:<20}  {:<9}  {:<7}  {:<7}",
                "id", "name", "slug", "kind", "public", "deleted"
            );
            for i in &instances {
                println!(
                    "{:>12}  {:<20}  {:<20}  {:<9}  {:<7}  {:<7}",
                    i.id,
                    i.name,
                    i.slug,
                    i.kind.as_str(),
                    bool_str(i.public_submission_enabled),
                    bool_str(i.deleted),
                );
            }
            println!("{} instance(s)", instances.len());
        }
        InstanceCmd::Update {
            id,
            name,
            from_name,
            signature,
            public_submission_enabled,
            deleted,
        } => {
            if let Some(deleted) = deleted {
                if dry_run {
                    println!("[dry-run] would set instance {id} deleted={deleted}");
                    return Ok(());
                }
                db.update_instance(&id, InstanceUpdateShape::SetDeleted(deleted))
                    .await?;
                println!("instance {id} deleted={deleted}");
                return Ok(());
            }
            let current = db
                .get_instances(&[&id])
                .await?
                .into_iter()
                .next()
                .flatten()
                .ok_or_else(|| anyhow!("no instance with id {id}"))?;
            let name = name.unwrap_or(current.name);
            let from_name = from_name.unwrap_or(current.from_name);
            let signature = signature.unwrap_or(current.signature);
            let public_submission_enabled =
                public_submission_enabled.unwrap_or(current.public_submission_enabled);
            if public_submission_enabled && current.kind != InstanceKind::Support {
                bail!("public_submission_enabled only applies to a support instance");
            }
            if dry_run {
                println!(
                    "[dry-run] would update instance {id}: name={name:?} from_name={from_name:?} \
                     signature={signature:?} public_submission_enabled={public_submission_enabled}"
                );
                return Ok(());
            }
            db.update_instance(
                &id,
                InstanceUpdateShape::Fields {
                    name: &name,
                    from_name: &from_name,
                    signature: &signature,
                    public_submission_enabled,
                },
            )
            .await?;
            println!("updated instance {id}");
        }
    }
    Ok(())
}

async fn run_address(db: &impl Handler, cmd: AddressCmd, dry_run: bool) -> Result<()> {
    match cmd {
        AddressCmd::Add { instance, address } => {
            let instance_id = db::resolve_instance_id(db, &instance).await?;
            let instance_rec = db
                .get_instances(&[&instance_id])
                .await?
                .into_iter()
                .next()
                .flatten()
                .ok_or_else(|| anyhow!("no instance with id {instance_id}"))?;
            db::require_instance_kind(&instance_rec, InstanceKind::Support)
                .map_err(|e| anyhow!(e))?;
            let instance = instance_id;
            let (normalized, kind) =
                routing::classify_for_storage(&address).map_err(|e| anyhow!(e))?;
            if let Some(existing) = db.get_inbound_address(&normalized).await? {
                return Err(anyhow!(
                    "{normalized} is already mapped to instance {}",
                    existing.instance_id
                ));
            }
            if dry_run {
                println!(
                    "[dry-run] would add {normalized} ({:?}) to instance {instance}",
                    kind
                );
                return Ok(());
            }
            let created = db
                .create_inbound_address(&normalized, &instance, kind)
                .await?;
            println!(
                "address: {}\ninstance_id: {}\nkind: {:?}\ncreated_at: {}",
                created.address, created.instance_id, created.kind, created.created_at
            );
        }
        AddressCmd::Remove { address } => {
            let normalized = address.trim().to_lowercase();
            let existing = db
                .get_inbound_address(&normalized)
                .await?
                .ok_or_else(|| anyhow!("no inbound address {normalized}"))?;
            if dry_run {
                println!(
                    "[dry-run] would remove {normalized} (instance {})",
                    existing.instance_id
                );
                return Ok(());
            }
            db.delete_inbound_address(&normalized).await?;
            println!("removed {normalized}");
        }
        AddressCmd::List { instance } => {
            let instance = db::resolve_instance_id(db, &instance).await?;
            let addrs = db.list_inbound_addresses_by_instance(&instance).await?;
            println!("{:<30}  {:<10}  {:<12}", "address", "kind", "created_at");
            for a in &addrs {
                println!("{:<30}  {:?}  {:<12}", a.address, a.kind, a.created_at);
            }
            println!("{} address(es)", addrs.len());
        }
    }
    Ok(())
}

async fn run_user(db: &impl Handler, cmd: UserCmd, dry_run: bool) -> Result<()> {
    match cmd {
        UserCmd::Create {
            email,
            name,
            superuser,
        } => {
            let email = db::normalize_user_email(&email).map_err(|e| anyhow!(e))?;
            if db.get_user_id_by_email(&email).await?.is_some() {
                return Err(anyhow!("a user with email {email} already exists"));
            }
            if dry_run {
                println!(
                    "[dry-run] would create user email={email:?} name={name:?} superuser={superuser}"
                );
                return Ok(());
            }
            let user = db.create_user(&email, &name).await?;
            if superuser {
                db.update_user(&user.id, UserUpdateShape::SetSuperuser(true))
                    .await?;
            }
            print_user(&User { superuser, ..user });
        }
        UserCmd::SetSuperuser { id_or_email, value } => {
            let user_id = resolve_user_id(db, &id_or_email).await?;
            if dry_run {
                println!("[dry-run] would set user {user_id} superuser={value}");
                return Ok(());
            }
            db.update_user(&user_id, UserUpdateShape::SetSuperuser(value))
                .await?;
            println!("user {user_id} superuser={value}");
        }
        UserCmd::List => {
            let users = db.list_users().await?;
            println!(
                "{:>12}  {:<30}  {:<20}  {:<7}  {:<9}",
                "id", "email", "name", "enabled", "superuser"
            );
            for u in &users {
                println!(
                    "{:<12}  {:<30}  {:<20}  {:<7}  {:<9}",
                    u.id,
                    u.email,
                    u.name,
                    bool_str(u.enabled),
                    bool_str(u.superuser)
                );
            }
            println!("{} user(s)", users.len());
        }
    }
    Ok(())
}

async fn run_member(db: &impl Handler, cmd: MemberCmd, dry_run: bool) -> Result<()> {
    match cmd {
        MemberCmd::Add {
            instance,
            user,
            role,
        } => {
            let instance = db::resolve_instance_id(db, &instance).await?;
            let user_id = resolve_user_id(db, &user).await?;
            let existing = db.list_memberships_by_user(&user_id).await?;
            if existing.iter().any(|m| m.instance_id == instance) {
                return Err(anyhow!(
                    "user {user_id} is already a member of instance {instance} — remove it first \
                     to change the role"
                ));
            }
            let role: MembershipRole = role.into();
            if dry_run {
                println!(
                    "[dry-run] would add user {user_id} to instance {instance} as {}",
                    role.as_str()
                );
                return Ok(());
            }
            let membership = db.create_membership(&user_id, &instance, role).await?;
            println!(
                "id: {}\nuser_id: {}\ninstance_id: {}\nrole: {}",
                membership.id,
                membership.user_id,
                membership.instance_id,
                membership.role.as_str()
            );
        }
        MemberCmd::Remove { instance, user } => {
            let instance = db::resolve_instance_id(db, &instance).await?;
            let user_id = resolve_user_id(db, &user).await?;
            let membership = db
                .list_memberships_by_user(&user_id)
                .await?
                .into_iter()
                .find(|m| m.instance_id == instance)
                .ok_or_else(|| anyhow!("user {user_id} is not a member of instance {instance}"))?;
            if dry_run {
                println!(
                    "[dry-run] would remove membership {} (user {user_id}, instance {instance})",
                    membership.id
                );
                return Ok(());
            }
            db.delete_membership(&membership.id).await?;
            println!("removed membership {}", membership.id);
        }
        MemberCmd::List { instance } => {
            let instance = db::resolve_instance_id(db, &instance).await?;
            let memberships = db.list_memberships_by_instance(&instance).await?;
            if memberships.is_empty() {
                println!("0 member(s)");
                return Ok(());
            }
            let user_ids: Vec<&str> = memberships.iter().map(|m| m.user_id.as_str()).collect();
            let users = db.get_users(&user_ids).await?;
            println!("{:>12}  {:<30}  {:<7}", "user_id", "email", "role");
            for (m, user) in memberships.iter().zip(users) {
                let email = user
                    .map(|u| u.email)
                    .unwrap_or_else(|| "<missing>".to_string());
                println!("{:<12}  {:<30}  {:<7}", m.user_id, email, m.role.as_str());
            }
            println!("{} member(s)", memberships.len());
        }
    }
    Ok(())
}

async fn run_mail(db: dynamodb::Handler, cmd: MailCmd, dry_run: bool) -> Result<()> {
    if dry_run {
        bail!("mail process does not support --dry-run (it always writes: a ticket/message)");
    }
    match cmd {
        MailCmd::Process { file, fresh } => {
            let raw =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            // Content-derived, not path-derived: two different fixture
            // files with the same bytes are (correctly) the same SES
            // message id; running the same file twice is how the
            // duplicate-delivery path gets exercised, per this
            // subcommand's doc comment.
            let ses_message_id = if fresh {
                format!("local-fresh-{}", nanoid::nanoid!(24))
            } else {
                format!("local-{}", sha256_hex(&raw))
            };

            let app = toolbox::app::new(
                db,
                toolbox::mockmail::Handler::from_env(),
                toolbox::mockstorage::Storage::from_env(),
                toolbox::mockqueue::Queue::new(),
                0,
            );

            // Mirror what SES's own S3 action would have written, so the
            // local path exercises exactly the same raw_s3_key plumbing the
            // Lambda does.
            let raw_key = format!("inbound/{ses_message_id}");
            {
                use toolbox::storage::Handler as _;
                app.storage
                    .put_bytes(&raw_key, &raw, "message/rfc822")
                    .await
                    .context("writing raw MIME to local mock storage")?;
            }

            let outcome = toolbox::inbound::pipeline::process_raw_message(
                &app,
                &ses_message_id,
                &raw,
                Some(&raw_key),
            )
            .await?;

            println!("ses_message_id: {ses_message_id}");
            match &outcome.dropped_reason {
                Some(reason) => println!("dropped: {reason}"),
                None => {
                    println!("ticket_id: {}", outcome.ticket_id.as_deref().unwrap_or("-"));
                    println!(
                        "message_id: {}",
                        outcome.message_id.as_deref().unwrap_or("-")
                    );
                    println!(
                        "created_new_ticket: {}",
                        bool_str(outcome.created_new_ticket)
                    );
                    println!("reopened: {}", bool_str(outcome.reopened));
                    println!(
                        "added_sender_as_cc: {}",
                        bool_str(outcome.added_sender_as_cc)
                    );
                    println!("attachment_count: {}", outcome.attachment_count);
                }
            }
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

async fn run_suppression(db: &impl Handler, cmd: SuppressionCmd, dry_run: bool) -> Result<()> {
    match cmd {
        SuppressionCmd::List => {
            println!(
                "{:<40}  {:<12}  {:<12}  {:>6}  detail",
                "email", "reason", "since", "events"
            );
            let mut cursor = None;
            let mut count = 0;
            loop {
                let page = db.scan_email_suppressions(cursor).await?;
                for row in page.rows {
                    match row {
                        Ok(s) => {
                            count += 1;
                            println!(
                                "{:<40}  {:<12}  {:<12}  {:>6}  {}",
                                s.email,
                                s.reason.as_str(),
                                s.suppressed_at,
                                s.event_count,
                                s.detail.as_deref().unwrap_or("")
                            );
                        }
                        Err(e) => eprintln!("unreadable row: {e}"),
                    }
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            println!("{count} suppressed address(es)");
        }
        SuppressionCmd::Remove { email } => {
            let normalized = email.trim().to_lowercase();
            if dry_run {
                println!("[dry-run] would stop suppressing {normalized}");
                return Ok(());
            }
            if !db.delete_email_suppression(&normalized).await? {
                bail!("{normalized} is not suppressed");
            }
            println!("{normalized} is no longer suppressed by Toolbox.");
            println!(
                "SES may still hold it on the account-level suppression list; to clear that too:\n  \
                 aws sesv2 delete-suppressed-destination --email-address {normalized}"
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    toolbox::load_cli_env();

    let cli = Cli::parse();

    let db_prefix = cli
        .db_prefix
        .clone()
        .or_else(|| std::env::var("DB_PREFIX").ok())
        .ok_or_else(|| anyhow!("DB_PREFIX is required (--db-prefix or the DB_PREFIX env var)"))?;

    // read_only mirrors --dry-run: even if a bug somewhere below tried to
    // write during a dry run, the handler itself would refuse.
    let db = dynamodb::Handler::new(&db_prefix, cli.dry_run).await;

    match cli.object {
        Object::Instance { cmd } => run_instance(&db, cmd, cli.dry_run).await,
        Object::Address { cmd } => run_address(&db, cmd, cli.dry_run).await,
        Object::User { cmd } => run_user(&db, cmd, cli.dry_run).await,
        Object::Member { cmd } => run_member(&db, cmd, cli.dry_run).await,
        Object::Mail { cmd } => run_mail(db, cmd, cli.dry_run).await,
        Object::Suppression { cmd } => run_suppression(&db, cmd, cli.dry_run).await,
    }
}
