// SPDX-License-Identifier: AGPL-3.0-only

//! Segments: named, saved predicates over contacts. The rule document is JSON; `rules` parses and
//! validates it into a tree that cannot represent an empty group, an unknown field or a node that
//! is both a rule and a group, and `compile` turns such a tree into parameterised SQL.

pub mod compile;
pub mod routes;
pub mod rules;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    custom_fields::{FieldKind, definitions_for},
    error::{ApiError, ApiResult},
};
use rules::Group;

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Segment {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    /// The rule document, `{match, rules}`. Replaced as a whole on every edit.
    #[schema(value_type = Object)]
    pub rules: Value,
    /// Starts at 1 and moves only when an edit changes `rules`: a rename leaves it alone.
    pub version: i32,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The attribute kinds a rule may be *read* against: archived definitions included. Archiving a
/// custom field keeps every contact's value, so it must not break a saved segment that names it.
/// For a segment already stored. A document being saved or previewed takes [`kinds_for_new_rules`].
pub async fn kinds_for_stored_rules(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
) -> sqlx::Result<HashMap<String, FieldKind>> {
    Ok(definitions_for(db, workspace_id)
        .await?
        .into_iter()
        .map(|d| (d.key, d.kind))
        .collect())
}

/// The attribute kinds a rule may be *written* against: archived definitions excluded, so a new
/// rule cannot name a field that no longer takes data. For a document being saved or previewed. A
/// stored segment takes [`kinds_for_stored_rules`]; the parser cannot tell the two apart.
pub async fn kinds_for_new_rules(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
) -> sqlx::Result<HashMap<String, FieldKind>> {
    Ok(definitions_for(db, workspace_id)
        .await?
        .into_iter()
        .filter(|d| d.archived_at.is_none())
        .map(|d| (d.key, d.kind))
        .collect())
}

/// Parses a rule document a caller is saving or previewing. Never builds a `Group` by hand: only
/// `Group::parse` can refuse an empty group or an unknown field before the compiler sees it.
pub async fn parse_new_rules(
    db: &mut PgConnection,
    workspace_id: Uuid,
    doc: &Value,
) -> ApiResult<Group> {
    let kinds = kinds_for_new_rules(&mut *db, workspace_id).await?;
    Ok(Group::parse(doc, &kinds)?)
}

/// The stored segment's rules as a parsed tree, `404` if it is not in this workspace. This is the
/// loader every later path that accepts a `segment_id` goes through: an id that resolves to
/// nothing is refused, never read as "no rules". The query carries its own `workspace_id`; the
/// compiler adds none.
pub async fn load_rules(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Group> {
    let doc = sqlx::query_scalar!(
        "select rules from segments where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let kinds = kinds_for_stored_rules(&mut *db, workspace_id).await?;
    // A stored document that no longer parses answers with the parser's own problem code.
    Ok(Group::parse(&doc, &kinds)?)
}
