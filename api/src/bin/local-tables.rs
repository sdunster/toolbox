//! Create the DynamoDB tables in a local DynamoDB (`local/dynamodb.sh start`, or
//! `make local-up`).
//!
//! The schema here is transcribed **by hand** from `infra/dynamodb.tf` — that
//! file, not this one, is the source of truth for the deployed tables (DynamoDB
//! Local has no Terraform provider, so there is no way to drive this off the
//! `.tf` file directly). This file must be updated whenever `infra/dynamodb.tf`
//! gains a table or GSI; drifting apart is the one maintenance cost of this
//! arrangement, and `--check` (wired to `make local-tables-check` in CI) is the
//! tripwire that catches it.
//!
//! Refuses to run unless the DynamoDB endpoint points at localhost (via
//! `local_dev::require_local_dynamodb_endpoint`), so a stray `DB_PREFIX` can
//! never create tables in a real AWS account.

use anyhow::{Result, anyhow, bail};
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection,
    ProjectionType, ScalarAttributeType, TimeToLiveSpecification,
};
use clap::Parser;

/// A key attribute and its DynamoDB scalar type.
#[derive(Clone, Copy)]
struct Attr(&'static str, ScalarKind);

#[derive(Clone, Copy, PartialEq)]
enum ScalarKind {
    S,
    N,
}

impl From<ScalarKind> for ScalarAttributeType {
    fn from(k: ScalarKind) -> Self {
        match k {
            ScalarKind::S => ScalarAttributeType::S,
            ScalarKind::N => ScalarAttributeType::N,
        }
    }
}

#[derive(Clone, Copy)]
struct Gsi {
    name: &'static str,
    hash: &'static str,
    range: Option<&'static str>,
    /// KEYS_ONLY when true, ALL when false. No table in this schema uses INCLUDE.
    keys_only: bool,
}

#[derive(Clone, Copy)]
struct Table {
    /// Suffix after `${DB_PREFIX}_`.
    name: &'static str,
    hash: &'static str,
    /// Every attribute used as a table or index key, with its type — this always
    /// includes the hash key itself. Everything else on an item is schema-free
    /// and not declared here; see SCHEMA.md.
    attrs: &'static [Attr],
    gsis: &'static [Gsi],
    /// TTL attribute, if the table has TTL enabled.
    ttl: Option<&'static str>,
}

use ScalarKind::{N, S};

const fn all(name: &'static str, hash: &'static str, range: Option<&'static str>) -> Gsi {
    Gsi {
        name,
        hash,
        range,
        keys_only: false,
    }
}

const fn keys_only(name: &'static str, hash: &'static str) -> Gsi {
    Gsi {
        name,
        hash,
        range: None,
        keys_only: true,
    }
}

/// The 16 tables, in the same order as `infra/dynamodb.tf`.
const TABLES: &[Table] = &[
    Table {
        name: "instance",
        hash: "id",
        attrs: &[Attr("id", S), Attr("slug", S)],
        gsis: &[keys_only("slug-index", "slug")],
        ttl: None,
    },
    Table {
        name: "inbound_address",
        hash: "address",
        attrs: &[Attr("address", S), Attr("instance_id", S)],
        gsis: &[all("instance_id-index", "instance_id", None)],
        ttl: None,
    },
    Table {
        name: "user",
        hash: "id",
        attrs: &[Attr("id", S), Attr("email", S)],
        gsis: &[keys_only("email-index", "email")],
        ttl: None,
    },
    Table {
        name: "membership",
        hash: "id",
        attrs: &[Attr("id", S), Attr("instance_id", S), Attr("user_id", S)],
        gsis: &[
            all("instance_id-index", "instance_id", None),
            all("user_id-index", "user_id", None),
        ],
        ttl: None,
    },
    Table {
        name: "ticket",
        hash: "id",
        attrs: &[
            Attr("id", S),
            Attr("instance_status", S),
            Attr("instance_visible", S),
            Attr("instance_assignee", S),
            Attr("last_activity_at", N),
            Attr("instance_number", S),
        ],
        gsis: &[
            all(
                "instance_status-last_activity_at-index",
                "instance_status",
                Some("last_activity_at"),
            ),
            all(
                "instance_visible-last_activity_at-index",
                "instance_visible",
                Some("last_activity_at"),
            ),
            all(
                "instance_assignee-last_activity_at-index",
                "instance_assignee",
                Some("last_activity_at"),
            ),
            keys_only("instance_number-index", "instance_number"),
        ],
        ttl: None,
    },
    Table {
        name: "ticket_message",
        hash: "id",
        attrs: &[
            Attr("id", S),
            Attr("ticket_id", S),
            Attr("created_at", N),
            Attr("rfc_message_id", S),
        ],
        gsis: &[
            all(
                "ticket_id-created_at-index",
                "ticket_id",
                Some("created_at"),
            ),
            keys_only("rfc_message_id-index", "rfc_message_id"),
        ],
        ttl: None,
    },
    Table {
        name: "counter",
        hash: "id",
        attrs: &[Attr("id", S)],
        gsis: &[],
        ttl: None,
    },
    Table {
        name: "login_code",
        hash: "email",
        attrs: &[Attr("email", S)],
        gsis: &[],
        ttl: Some("expires_at"),
    },
    Table {
        name: "user_token",
        hash: "id",
        attrs: &[Attr("id", S)],
        gsis: &[],
        ttl: None,
    },
    Table {
        name: "oauth_grant",
        hash: "id",
        attrs: &[Attr("id", S), Attr("user_id", S)],
        gsis: &[all("user_id-index", "user_id", None)],
        ttl: Some("expires_at"),
    },
    Table {
        name: "api_token",
        hash: "id",
        attrs: &[Attr("id", S), Attr("instance_id", S)],
        gsis: &[all("instance_id-index", "instance_id", None)],
        ttl: None,
    },
    Table {
        name: "project",
        hash: "id",
        attrs: &[Attr("id", S), Attr("instance_id", S)],
        gsis: &[all("instance_id-index", "instance_id", None)],
        ttl: None,
    },
    Table {
        name: "billable_item",
        hash: "id",
        attrs: &[
            Attr("id", S),
            Attr("instance_id", S),
            Attr("project_id", S),
            Attr("date", S),
        ],
        gsis: &[
            all("instance_id-date-index", "instance_id", Some("date")),
            all("project_id-date-index", "project_id", Some("date")),
        ],
        ttl: None,
    },
    Table {
        name: "invoice",
        hash: "id",
        attrs: &[
            Attr("id", S),
            Attr("instance_id", S),
            Attr("project_id", S),
            Attr("created_at", N),
        ],
        gsis: &[
            all(
                "instance_id-created_at-index",
                "instance_id",
                Some("created_at"),
            ),
            all(
                "project_id-created_at-index",
                "project_id",
                Some("created_at"),
            ),
        ],
        ttl: None,
    },
    Table {
        name: "webauthn_credential",
        hash: "id",
        attrs: &[Attr("id", S), Attr("user_id", S)],
        gsis: &[all("user_id-index", "user_id", None)],
        ttl: None,
    },
    Table {
        name: "ephemeral_state",
        hash: "id",
        attrs: &[Attr("id", S)],
        gsis: &[],
        ttl: Some("expires_at"),
    },
    Table {
        name: "processed_message",
        hash: "ses_message_id",
        attrs: &[Attr("ses_message_id", S)],
        gsis: &[],
        ttl: Some("expires_at"),
    },
];

#[derive(Parser)]
#[command(about = "Create the Toolbox tables in a local DynamoDB")]
struct Cli {
    /// Delete and recreate every table, discarding all local data.
    #[arg(long)]
    recreate: bool,

    /// Don't create anything; report which tables are missing and exit non-zero if any are.
    #[arg(long)]
    check: bool,
}

async fn create(client: &Client, prefix: &str, table: &Table) -> Result<()> {
    let full_name = format!("{prefix}_{}", table.name);
    let mut req = client
        .create_table()
        .table_name(&full_name)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name(table.hash)
                .key_type(KeyType::Hash)
                .build()?,
        );
    for Attr(name, kind) in table.attrs {
        req = req.attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name(*name)
                .attribute_type(ScalarAttributeType::from(*kind))
                .build()?,
        );
    }
    for gsi in table.gsis {
        let mut index = GlobalSecondaryIndex::builder()
            .index_name(gsi.name)
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(gsi.hash)
                    .key_type(KeyType::Hash)
                    .build()?,
            )
            .projection(
                Projection::builder()
                    .projection_type(if gsi.keys_only {
                        ProjectionType::KeysOnly
                    } else {
                        ProjectionType::All
                    })
                    .build(),
            );
        if let Some(range) = gsi.range {
            index = index.key_schema(
                KeySchemaElement::builder()
                    .attribute_name(range)
                    .key_type(KeyType::Range)
                    .build()?,
            );
        }
        req = req.global_secondary_indexes(index.build()?);
    }
    req.send().await?;

    if let Some(ttl_attr) = table.ttl {
        client
            .update_time_to_live()
            .table_name(&full_name)
            .time_to_live_specification(
                TimeToLiveSpecification::builder()
                    .attribute_name(ttl_attr)
                    .enabled(true)
                    .build()?,
            )
            .send()
            .await?;
    }
    println!("created {full_name}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    toolbox::load_cli_env();
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint()?;
    let prefix = std::env::var("DB_PREFIX").map_err(|_| anyhow!("DB_PREFIX must be set"))?;

    let client = toolbox::local_dev::dynamodb_client().await;

    let existing: Vec<String> = client
        .list_tables()
        .send()
        .await?
        .table_names
        .unwrap_or_default();
    println!(
        "{endpoint} has {} table(s), prefix {prefix}_",
        existing.len()
    );

    if cli.check {
        let missing: Vec<String> = TABLES
            .iter()
            .map(|t| format!("{prefix}_{}", t.name))
            .filter(|n| !existing.contains(n))
            .collect();
        if missing.is_empty() {
            println!("all {} tables present", TABLES.len());
            return Ok(());
        }
        bail!("missing {} table(s): {}", missing.len(), missing.join(", "));
    }

    for table in TABLES {
        let full_name = format!("{prefix}_{}", table.name);
        if existing.contains(&full_name) {
            if !cli.recreate {
                println!("exists  {full_name}");
                continue;
            }
            client.delete_table().table_name(&full_name).send().await?;
            println!("dropped {full_name}");
        }
        create(&client, &prefix, table).await?;
    }
    Ok(())
}
