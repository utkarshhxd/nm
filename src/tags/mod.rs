// SPDX-License-Identifier: AGPL-3.0-only

//! Tags: labels on a contact. Unlike a list, a tag is never a send target and carries no
//! subscription state. Tags organise; lists receive mail.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

pub mod routes;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Tag {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// The tag, or `404` if it is not in this workspace.
pub async fn get(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Tag> {
    sqlx::query_as!(
        Tag,
        "select id, name, created_at from tags where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Puts the tag on the contact; `true` if it was not on it already. Applying twice is one row and
/// still succeeds. The caller passes the transaction and has already authorised the actor. `404`
/// if either id is not in this workspace.
pub async fn apply(
    db: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
    tag_id: Uuid,
) -> ApiResult<bool> {
    let done = sqlx::query!(
        "insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3) \
         on conflict (contact_id, tag_id) do nothing",
        workspace_id,
        contact_id,
        tag_id,
    )
    .execute(db)
    .await
    // Both keys are composite on the workspace, so a contact or a tag of another workspace (or
    // none) is the only way for one to fail.
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_foreign_key_violation() => ApiError::NotFound,
        _ => e.into(),
    })?;
    Ok(done.rows_affected() == 1)
}

/// Takes the tag off the contact. `404` if it was not on it, or either is not in this workspace.
pub async fn remove(
    db: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
    tag_id: Uuid,
) -> ApiResult<()> {
    let done = sqlx::query!(
        "delete from contact_tags where workspace_id = $1 and contact_id = $2 and tag_id = $3",
        workspace_id,
        contact_id,
        tag_id,
    )
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(())
}
