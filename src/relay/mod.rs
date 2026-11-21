// SPDX-License-Identifier: AGPL-3.0-only

//! Relays: the SMTP servers a workspace's mail leaves through. This module is what the sender
//! reads; `routes` is what an operator configures them with.
//!
//! **A relay's password is held to one rule (spec §8, invariant 7): it appears in no API response,
//! no log line and no audit row.** It lives in `relays.password_encrypted` as a `Secrets` blob and
//! comes back out through exactly one function, [`password`], whose caller is the SMTP transport.
//! [`Relay`] has no field for it, so a `Debug` or `Serialize` of a relay cannot carry it.

use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{crypto::secrets::Secrets, error::ApiResult};

pub mod routes;

/// How the connection to the relay is secured. Stored as text under a `check` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Security {
    /// TLS from the first byte (SMTPS, usually port 465).
    ImplicitTls,
    /// Plain connect, then upgrade (usually port 587). Renamed because `snake_case` would spell
    /// it `start_tls`, and the column's check constraint says `starttls`.
    #[serde(rename = "starttls")]
    #[sqlx(rename = "starttls")]
    StartTls,
}

/// A relay as the sender needs it: everything but the password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub host: String,
    pub port: i32,
    pub security: Security,
    pub username: Option<String>,
    pub max_connections: i32,
    pub rate_limit_per_minute: i32,
    pub enabled: bool,
    pub failover_order: i32,
}

/// The enabled relays of a workspace in the order the sender tries them: `failover_order`, then
/// `id`, so the order is total. Matches the partial index `relays_failover`.
pub async fn enabled_relays(db: impl PgExecutor<'_>, workspace_id: Uuid) -> ApiResult<Vec<Relay>> {
    Ok(sqlx::query_as!(
        Relay,
        r#"select id, workspace_id, name, host, port, security as "security: Security", username,
                  max_connections, rate_limit_per_minute, enabled, failover_order
           from relays
           where workspace_id = $1 and enabled
           order by failover_order, id"#,
        workspace_id,
    )
    .fetch_all(db)
    .await?)
}

/// The relay's decrypted password, or `None` if it has none. `404` if the relay is not in this
/// workspace. The only place a stored password is turned back into text.
pub async fn password(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    relay_id: Uuid,
) -> ApiResult<Option<String>> {
    let blob = sqlx::query_scalar!(
        "select password_encrypted from relays where workspace_id = $1 and id = $2",
        workspace_id,
        relay_id,
    )
    .fetch_one(db)
    .await?;
    let Some(blob) = blob else {
        return Ok(None);
    };
    let plain = secrets
        .decrypt(&blob)
        .map_err(|e| anyhow::anyhow!("relay {relay_id}: {e}"))?;
    // Not `?` on `String::from_utf8`: its error owns the bytes and `{:?}` prints them, which is
    // how an `Internal` error's cause would reach the log.
    String::from_utf8(plain)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("relay {relay_id}: stored password is not valid UTF-8").into())
}
