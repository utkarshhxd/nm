// SPDX-License-Identifier: AGPL-3.0-only

//! Segments: named, saved predicates over contacts. The rule document is JSON; `rules` parses and
//! validates it into a tree that cannot represent an empty group, an unknown field or a node that
//! is both a rule and a group, and `compile` turns such a tree into parameterised SQL.

pub mod compile;
pub mod preview;
pub mod routes;
pub mod rules;

use std::collections::{HashMap, HashSet};

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
use rules::{Group, Refs, RuleError};

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

/// The list and tag ids a rule may name, which is every one this workspace has.
///
/// No `for share`, unlike the definitions: there is no purge guard to lose a race to, and a tag
/// deleted the instant after a save resolved it leaves the segment exactly where a tag deleted a
/// minute later leaves it — unreadable until it is saved again, which is [`Refs`]'s whole point.
async fn lists_and_tags(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
) -> sqlx::Result<(HashSet<Uuid>, HashSet<Uuid>)> {
    let row = sqlx::query!(
        r#"select
             coalesce((select array_agg(l.id) from lists l where l.workspace_id = $1),
                      '{}'::uuid[]) as "lists!: Vec<Uuid>",
             coalesce((select array_agg(t.id) from tags t where t.workspace_id = $1),
                      '{}'::uuid[]) as "tags!: Vec<Uuid>""#,
        workspace_id,
    )
    .fetch_one(db)
    .await?;
    Ok((
        row.lists.into_iter().collect(),
        row.tags.into_iter().collect(),
    ))
}

/// What a rule may be *read* against: archived definitions included. Archiving a custom field
/// keeps every contact's value, so it must not break a saved segment that names it. For a segment
/// already stored. A document being saved or previewed takes [`refs_for_new_rules`].
pub async fn refs_for_stored_rules(
    db: &mut PgConnection,
    workspace_id: Uuid,
) -> sqlx::Result<Refs> {
    let kinds = definitions_for(&mut *db, workspace_id)
        .await?
        .into_iter()
        .map(|d| (d.key, d.kind))
        .collect();
    let (lists, tags) = lists_and_tags(&mut *db, workspace_id).await?;
    Ok(Refs { kinds, lists, tags })
}

/// What a rule may be *written* against: archived definitions excluded, so a new rule cannot name
/// a field that no longer takes data. For a document being saved or previewed. A stored segment
/// takes [`refs_for_stored_rules`]; the parser cannot tell the two apart.
///
/// The definitions are read `for share`, held until the caller's transaction ends. Archiving a
/// field needs the row exclusively, so an archive (and the purge that can only follow it) waits
/// for a save in flight and its purge guard then sees the saved segment. Without the lock a save
/// could resolve a live field, lose the race to an archive and a purge, and commit a segment
/// naming a field that is gone, with no repair path. `for share` is shared: savers never wait on
/// each other. Outside a transaction (a preview) it is released at once, which is all a read needs.
pub async fn refs_for_new_rules(db: &mut PgConnection, workspace_id: Uuid) -> sqlx::Result<Refs> {
    let kinds = sqlx::query!(
        r#"select key, kind as "kind: FieldKind" from custom_fields
           where workspace_id = $1 and archived_at is null for share"#,
        workspace_id,
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|r| (r.key, r.kind))
    .collect();
    let (lists, tags) = lists_and_tags(&mut *db, workspace_id).await?;
    Ok(Refs { kinds, lists, tags })
}

/// Parses a rule document a caller is saving or previewing. Never builds a `Group` by hand: only
/// `Group::parse` can refuse an empty group or an unknown field before the compiler sees it.
pub async fn parse_new_rules(
    db: &mut PgConnection,
    workspace_id: Uuid,
    doc: &Value,
) -> ApiResult<Group> {
    let refs = refs_for_new_rules(&mut *db, workspace_id).await?;
    Ok(Group::parse(doc, &refs)?)
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
    let refs = refs_for_stored_rules(&mut *db, workspace_id).await?;
    Ok(Group::parse(doc, &refs)?)
}

/// The stored segment's rules as a parsed tree, `404` if it is not in this workspace. This is the
/// loader every later path that accepts a `segment_id` goes through: an id that resolves to
/// nothing is refused, never read as "no rules". The query carries its own `workspace_id`; the
/// compiler adds none.
///
/// The row and everything it resolves against come from ONE statement, so they share a snapshot: a
/// field archived — or a tag deleted — between two statements cannot decide whether a stored
/// segment loads. Every definition is read, archived ones included.
pub async fn load_rules(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Group> {
    let (_, parsed) = load_many_rules(db, workspace_id, &[id])
        .await?
        .pop()
        .ok_or(ApiError::NotFound)?;
    // One document, so a document that no longer parses is this caller's whole answer.
    Ok(parsed?)
}

/// [`load_rules`] for several ids at once, in the order asked for, in **one** statement.
///
/// The list view counts every segment on the page, and one request per row spends a workspace's
/// whole preview quota on a single page load. Counting them together needs their rules together.
///
/// Every id must resolve: a missing one is the same `404` as on every other path that accepts a
/// `segment_id`, never a silently shorter answer. Duplicates are the caller's to remove.
///
/// **A document that no longer parses is returned as that row's `Err`, not as the batch's.** One
/// unreadable segment on a page used to cost the other twenty-four their counts; the caller
/// decides what to do per row, and `load_rules` — one id — simply propagates it.
pub async fn load_many_rules(
    db: &mut PgConnection,
    workspace_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<Vec<(Uuid, Result<Group, RuleError>)>> {
    let rows = sqlx::query!(
        r#"select s.id, s.rules,
                  coalesce((select jsonb_object_agg(f.key, f.kind) from custom_fields f
                            where f.workspace_id = $1), '{}'::jsonb)
                      as "kinds!: Json<HashMap<String, FieldKind>>",
                  coalesce((select array_agg(l.id) from lists l where l.workspace_id = $1),
                           '{}'::uuid[]) as "lists!: Vec<Uuid>",
                  coalesce((select array_agg(t.id) from tags t where t.workspace_id = $1),
                           '{}'::uuid[]) as "tags!: Vec<Uuid>"
           from segments s where s.workspace_id = $1 and s.id = any($2)"#,
        workspace_id,
        ids,
    )
    .fetch_all(&mut *db)
    .await?;
    let mut parsed = HashMap::with_capacity(rows.len());
    for row in rows {
        let refs = Refs {
            kinds: row.kinds.0,
            lists: row.lists.into_iter().collect(),
            tags: row.tags.into_iter().collect(),
        };
        parsed.insert(row.id, Group::parse(&row.rules, &refs));
    }
    ids.iter()
        .map(|id| {
            parsed
                .remove(id)
                .map(|group| (*id, group))
                .ok_or(ApiError::NotFound)
        })
        .collect()
}
