// SPDX-License-Identifier: AGPL-3.0-only

//! Sending domains: the from-domains a workspace may send as, with their DKIM settings and the
//! DNS checker's last verdict. This module is what the sender reads; `routes` is what an operator
//! configures them with and `dns` is the checker.
//!
//! **A domain's DKIM private key is held to the rule a relay's password is (spec §8, invariant
//! 7): it appears in no API response, no log line and no audit row.** It lives in
//! `sending_domains.dkim_private_key_encrypted` as a `Secrets` blob and comes back out through
//! exactly one function, [`dkim_private_key`], whose caller is the signer. [`SendingDomain`] has
//! no field for it, so a `Debug` or `Serialize` of one cannot carry it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, types::Json};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{crypto::secrets::Secrets, error::ApiResult};

pub mod dns;
pub mod routes;

/// Who signs the domain's mail with DKIM. Stored as text under a `check` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Signing {
    /// The relay signs and we hold no key.
    Relay,
    /// We sign, with the key stored for this domain.
    App,
}

/// One check's verdict. On the wire (and in `sending_domains.checks`) it is
/// `{"status": "pass"}`, `{"status": "fail", "reason": "..."}` or `{"status": "not_checked"}`.
///
/// A `Fail` reason is shown to an operator and stored, so it must never carry key material: a
/// mismatched DKIM `p=` says that it differs and names neither value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Fail(String),
    NotChecked,
}

/// The four checks. Every field defaults to `NotChecked`, so the `{}` a new domain starts with
/// and a document written before a check existed both read back as "not run".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct DnsChecks {
    pub spf: CheckResult,
    pub dkim: CheckResult,
    pub dmarc: CheckResult,
    pub return_path: CheckResult,
}

impl Default for DnsChecks {
    fn default() -> DnsChecks {
        DnsChecks {
            spf: CheckResult::NotChecked,
            dkim: CheckResult::NotChecked,
            dmarc: CheckResult::NotChecked,
            return_path: CheckResult::NotChecked,
        }
    }
}

/// A sending domain as the sender and the checker need it: everything but the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendingDomain {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// Lower case; the form the API stores.
    pub domain: String,
    pub from_name: Option<String>,
    pub signing: Signing,
    pub dkim_selector: String,
    pub return_path_domain: Option<String>,
    pub checks: DnsChecks,
    pub checked_at: Option<DateTime<Utc>>,
}

/// The workspace's sending domain for `domain`, or `None` if it has none: what a campaign asks
/// before it may use a from-address. Case-insensitive, because a mailbox's domain is.
pub async fn for_domain(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
    domain: &str,
) -> ApiResult<Option<SendingDomain>> {
    let row = sqlx::query!(
        r#"select id, workspace_id, domain, from_name, signing as "signing: Signing",
                  dkim_selector, return_path_domain, checks as "checks: Json<DnsChecks>",
                  checked_at
           from sending_domains where workspace_id = $1 and domain = $2"#,
        workspace_id,
        domain.to_ascii_lowercase(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| SendingDomain {
        id: r.id,
        workspace_id: r.workspace_id,
        domain: r.domain,
        from_name: r.from_name,
        signing: r.signing,
        dkim_selector: r.dkim_selector,
        return_path_domain: r.return_path_domain,
        checks: r.checks.0,
        checked_at: r.checked_at,
    }))
}

/// The domain's decrypted DKIM private key as DER, or `None` if it has none. `404` if the domain
/// is not in this workspace. The only place a stored key is turned back into bytes.
pub async fn dkim_private_key(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    id: Uuid,
) -> ApiResult<Option<Vec<u8>>> {
    let blob = sqlx::query_scalar!(
        "select dkim_private_key_encrypted from sending_domains \
         where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await?;
    blob.map(|b| {
        secrets
            .decrypt(&b)
            .map_err(|e| anyhow::anyhow!("sending domain {id}: {e}").into())
    })
    .transpose()
}
