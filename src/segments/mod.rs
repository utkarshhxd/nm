// SPDX-License-Identifier: AGPL-3.0-only

//! Segments: named, saved predicates over contacts. The rule document is JSON; `rules` parses and
//! validates it into a tree that cannot represent an empty group, an unknown field or a node that
//! is both a rule and a group, and `compile` turns such a tree into parameterised SQL.

pub mod compile;
pub mod preview;
pub mod routes;
pub mod rules;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor, types::Json};
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
///
/// The rows are read `for share`, held until the caller's transaction ends. Archiving a field
/// needs the row exclusively, so an archive (and the purge that can only follow it) waits for a
/// save in flight and its purge guard then sees the saved segment. Without the lock a save could
/// resolve a live field, lose the race to an archive and a purge, and commit a segment naming a
/// field that is gone, with no repair path. `for share` is shared: savers never wait on each
/// other. Outside a transaction (a preview) it is released at once, which is all a read needs.
pub async fn kinds_for_new_rules(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
) -> sqlx::Result<HashMap<String, FieldKind>> {
    Ok(sqlx::query!(
        r#"select key, kind as "kind: FieldKind" from custom_fields
           where workspace_id = $1 and archived_at is null for share"#,
        workspace_id,
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| (r.key, r.kind))
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

/// Parses a document the caller has just found to be identical to the stored one. It resolves like
/// a stored segment, archived definitions included: the rule that names an archived field is not
/// new, and refusing the document would make a rename that resends the whole object a `422`.
/// The caller owns the comparison; anything that differs from what is stored takes
/// [`parse_new_rules`].
pub async fn parse_unchanged_rules(
    db: &mut PgConnection,
    workspace_id: Uuid,
    doc: &Value,
) -> ApiResult<Group> {
    let kinds = kinds_for_stored_rules(&mut *db, workspace_id).await?;
    Ok(Group::parse(doc, &kinds)?)
}

/// The stored segment's rules as a parsed tree, `404` if it is not in this workspace. This is the
/// loader every later path that accepts a `segment_id` goes through: an id that resolves to
/// nothing is refused, never read as "no rules". The query carries its own `workspace_id`; the
/// compiler adds none.
///
/// The row and the definitions come from ONE statement, so they share a snapshot: a field archived
/// between two statements cannot make a stored segment fail to load. Every definition is read,
/// archived ones included.
pub async fn load_rules(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Group> {
    let row = sqlx::query!(
        r#"select s.rules,
                  coalesce((select jsonb_object_agg(f.key, f.kind) from custom_fields f
                            where f.workspace_id = $1), '{}'::jsonb)
                      as "kinds!: Json<HashMap<String, FieldKind>>"
           from segments s where s.workspace_id = $1 and s.id = $2"#,
        workspace_id,
        id,
    )
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::NotFound)?;
    // A stored document that no longer parses answers with the parser's own problem code.
    Ok(Group::parse(&row.rules, &row.kinds.0)?)
}
