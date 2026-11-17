// SPDX-License-Identifier: AGPL-3.0-only

//! The CSV importer: the inverse of [`super::export`].
//!
//! The first half of the pipeline is here: the upload (`routes`) and what the server makes of the
//! file's head (`sniff`). The types below are the contract every later stage takes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::custom_fields::Definition;

pub mod csv;
pub mod routes;
pub mod sniff;

/// A built-in contact column an import may write: the exporter's own header for it. `id`,
/// `created_at`, `updated_at` and `suppressed` are not here on purpose: the first three are the
/// database's, and an import may not bypass or lift a suppression (spec 7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContactField {
    Email,
    FirstName,
    LastName,
    Timezone,
    Tags,
    Lists,
}

impl ContactField {
    pub const ALL: [ContactField; 6] = [
        ContactField::Email,
        ContactField::FirstName,
        ContactField::LastName,
        ContactField::Timezone,
        ContactField::Tags,
        ContactField::Lists,
    ];

    /// The column header the exporter writes for it, which is also its wire name.
    pub fn header(self) -> &'static str {
        match self {
            ContactField::Email => "email",
            ContactField::FirstName => "first_name",
            ContactField::LastName => "last_name",
            ContactField::Timezone => "timezone",
            ContactField::Tags => "tags",
            ContactField::Lists => "lists",
        }
    }
}

/// What one CSV column is written to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ColumnTarget {
    /// The column is not imported. Also what a proposal says when it is unsure.
    Ignore,
    Field(ContactField),
    /// A custom field, by its key.
    Custom(String),
}

/// What to do with a row whose address is already a contact in the workspace (spec 7.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Dedup {
    /// Update the contact field by field; a blank cell leaves the stored value alone.
    #[default]
    UpdateExisting,
    SkipExisting,
    FailOnDuplicate,
}

/// One import, as every stage after the upload takes it.
#[derive(Debug, Clone)]
pub struct ImportJob {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// Always [`upload_path`] of `id`; never anything the client sent.
    pub path: PathBuf,
    pub delimiter: u8,
    pub has_header: bool,
    /// One target per CSV column, in column order.
    pub mapping: Vec<ColumnTarget>,
    pub dedup: Dedup,
    pub list_id: Option<Uuid>,
    /// The workspace's custom field definitions, archived ones included.
    pub defs: Vec<Definition>,
}

/// Where an import's uploaded file lives. Derived from the import id alone: a client-supplied
/// filename is an attacker-supplied path, so it is stored as a label and never reaches the
/// filesystem.
pub fn upload_path(id: Uuid) -> PathBuf {
    std::env::temp_dir().join(format!("nm-import-{id}.csv"))
}
