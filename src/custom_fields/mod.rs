// SPDX-License-Identifier: AGPL-3.0-only

//! Custom field definitions: the typed, workspace-scoped schema that a contact's `attributes`
//! are validated against. Archiving a field keeps every contact's value; only an explicit purge
//! removes them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, types::Json};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::FieldError;

pub mod routes;

/// How a field's value is typed, stored as snake_case text (a `check` constraint, not an enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Number,
    Date,
    // snake_case would spell it `date_time`; the check constraint and the spec say `datetime`.
    #[serde(rename = "datetime")]
    #[sqlx(rename = "datetime")]
    DateTime,
    Boolean,
    Select,
    MultiSelect,
}

impl FieldKind {
    fn has_options(self) -> bool {
        matches!(self, FieldKind::Select | FieldKind::MultiSelect)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Definition {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    /// The allowed values of a `select` or `multi_select` field; empty for every other kind.
    pub options: Vec<String>,
    /// Set when the field was deleted. Contacts keep their values until the field is purged.
    pub archived_at: Option<DateTime<Utc>>,
}

/// Every definition in the workspace, archived ones included: existing values must stay readable
/// and typed. A caller validating a NEW value must reject any field whose `archived_at` is set,
/// or archiving stops protecting anything and the field keeps taking fresh data.
pub async fn definitions_for(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
) -> sqlx::Result<Vec<Definition>> {
    sqlx::query_as!(
        Row,
        r#"select id, key, label, kind as "kind: FieldKind", options as "options: Json<Vec<String>>",
                  archived_at
           from custom_fields where workspace_id = $1 order by id"#,
        workspace_id,
    )
    .fetch_all(db)
    .await
    .map(|rows| rows.into_iter().map(Definition::from).collect())
}

/// A `custom_fields` row as the database returns it; `options` is `jsonb`.
pub(crate) struct Row {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    pub options: Json<Vec<String>>,
    pub archived_at: Option<DateTime<Utc>>,
}

impl From<Row> for Definition {
    fn from(r: Row) -> Definition {
        Definition {
            id: r.id,
            key: r.key,
            label: r.label,
            kind: r.kind,
            options: r.options.0,
            archived_at: r.archived_at,
        }
    }
}

/// `^[a-z][a-z0-9_]{0,62}$`, written out. The key is the JSONB key every value is stored under.
pub fn validate_key(raw: &str) -> Result<String, FieldError> {
    let b = raw.as_bytes();
    if (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        return Ok(raw.to_owned());
    }
    Err(FieldError::new(
        "key",
        "must be 1-63 characters: a lowercase letter, then lowercase letters, digits and underscores",
    ))
}

/// Trimmed, 1..=100 characters, no control characters.
pub(crate) fn validate_label(raw: &str) -> Result<String, FieldError> {
    let label = raw.trim();
    if (1..=100).contains(&label.chars().count()) && !label.chars().any(char::is_control) {
        return Ok(label.to_owned());
    }
    Err(FieldError::new(
        "label",
        "must be 1-100 characters with no control characters",
    ))
}

const MAX_OPTIONS: usize = 200;

/// A `select` or `multi_select` needs 1..=200 distinct options of 1..=100 characters each; every
/// other kind takes none. Returns the options trimmed.
pub(crate) fn validate_options(kind: FieldKind, raw: &[String]) -> Result<Vec<String>, FieldError> {
    let err = |m: &str| Err(FieldError::new("options", m));
    if !kind.has_options() {
        return if raw.is_empty() {
            Ok(vec![])
        } else {
            err("only a select or multi_select field has options")
        };
    }
    if !(1..=MAX_OPTIONS).contains(&raw.len()) {
        return err("a select or multi_select field needs 1-200 options");
    }
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for o in raw {
        let o = o.trim();
        if !(1..=100).contains(&o.chars().count()) || o.chars().any(char::is_control) {
            return err("each option must be 1-100 characters with no control characters");
        }
        if out.iter().any(|seen| seen == o) {
            return err("options must be distinct");
        }
        out.push(o.to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        for ok in ["a", "company_size", "a1_", &"a".repeat(63)] {
            assert_eq!(validate_key(ok).unwrap(), ok);
        }
        for bad in ["", "1a", "_a", "A", "a-b", "a b", "é", &"a".repeat(64)] {
            assert!(validate_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn options_follow_the_kind() {
        let s = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(validate_options(FieldKind::Text, &[]).unwrap().is_empty());
        assert!(validate_options(FieldKind::Text, &s(&["a"])).is_err());
        assert!(validate_options(FieldKind::Select, &[]).is_err());
        assert!(validate_options(FieldKind::MultiSelect, &s(&["a", "a"])).is_err());
        assert!(validate_options(FieldKind::Select, &s(&["  "])).is_err());
        assert_eq!(
            validate_options(FieldKind::Select, &s(&[" a ", "b"])).unwrap(),
            s(&["a", "b"])
        );
    }
}
