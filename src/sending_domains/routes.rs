// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, PgExecutor, types::Json as SqlJson};
use utoipa::ToSchema;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{DnsChecks, SendingDomain, Signing, dns};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/sending-domains",
            axum::routing::get(list_sending_domains).post(create_sending_domain),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/sending-domains/{id}",
            axum::routing::get(get_sending_domain)
                .patch(update_sending_domain)
                .delete(delete_sending_domain),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/sending-domains/{id}/check",
            axum::routing::post(check_sending_domain),
        )
}

/// A sending domain as the API shows it. There is no key field, on purpose: `has_dkim_key` is all
/// an operator may learn about it.
#[derive(Serialize, ToSchema)]
pub struct SendingDomainView {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// Lower case. Unique in the workspace, and fixed once created.
    pub domain: String,
    /// The display name a campaign from this domain defaults to.
    pub from_name: Option<String>,
    pub signing: Signing,
    pub dkim_selector: String,
    /// Whether a DKIM private key is stored. The key itself is never returned.
    pub has_dkim_key: bool,
    pub return_path_domain: Option<String>,
    /// The DNS checker's last result; every check is `not_checked` until it has run.
    pub checks: DnsChecks,
    /// When `checks` was written; null if the checker has never run for this configuration.
    pub checked_at: Option<DateTime<Utc>>,
}

// Neither request type derives `Debug`, so a `{:?}` of one cannot put the key in a log.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSendingDomain {
    /// The domain mail is sent from, as in `news@example.com`'s `example.com`. ASCII (punycode for
    /// an internationalized name); stored lower case.
    pub domain: String,
    /// Display name a campaign from this domain defaults to.
    pub from_name: Option<String>,
    /// `relay` if the relay signs, `app` if this product does.
    pub signing: Signing,
    /// Where the DKIM key is published, as `{selector}._domainkey.{domain}`. Default `nm1`.
    pub dkim_selector: Option<String>,
    /// A domain whose CNAME carries bounces to the relay. The sending domain or a subdomain of it.
    pub return_path_domain: Option<String>,
    /// An unencrypted RSA private key in PEM (`BEGIN PRIVATE KEY` or `BEGIN RSA PRIVATE KEY`).
    /// Required when `signing` is `app`. Stored encrypted and never returned.
    pub dkim_private_key: Option<String>,
}

/// A field left out is left alone. `from_name`, `return_path_domain` and `dkim_private_key` may be
/// `null`, which clears them; on any other field `null` is `422`, because there is nothing to
/// clear it to. The domain itself cannot be changed: delete the domain and add the new one.
///
/// Changing `signing`, `dkim_selector`, `return_path_domain` or `dkim_private_key` resets `checks`,
/// which no longer describe what is configured.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateSendingDomain {
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub from_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Signing, required = false)]
    pub signing: Option<Option<Signing>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub dkim_selector: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub return_path_domain: Option<Option<String>>,
    /// Absent keeps the stored key; `null` removes it; a PEM string replaces it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub dkim_private_key: Option<Option<String>>,
}

/// Tells a key that is absent (`None`) from one present as `null` (`Some(None)`). A plain
/// `Option<T>` maps both to `None`, which here would turn `"dkim_private_key": null` into "keep
/// it". The same helper `relay`, `lists`, `contacts`, `forms` and `segments` each carry.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// Collects a field's refusal instead of returning at the first, so one `422` names them all.
fn keep<T>(r: Result<T, FieldError>, errors: &mut Vec<FieldError>) -> Option<T> {
    r.map_err(|e| errors.push(e)).ok()
}

/// A key sent as `null` where null means nothing.
fn not_null<T>(
    field: &'static str,
    v: Option<Option<T>>,
    errors: &mut Vec<FieldError>,
) -> Option<T> {
    if matches!(v, Some(None)) {
        errors.push(FieldError::new(field, "cannot be null"));
    }
    v.flatten()
}

/// A DNS name as the checker will look it up: ASCII labels of letters, digits and hyphens, none
/// starting or ending with one, at most 63 long, 253 in all; lower-cased; no trailing dot.
/// `min_labels` is 2 for a domain (a bare `com` is not somewhere to send from) and 1 for a
/// selector.
fn dns_name(field: &'static str, v: &str, min_labels: usize) -> Result<String, FieldError> {
    let v = v.trim().to_ascii_lowercase();
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if v.len() > 253 || v.split('.').count() < min_labels || !v.split('.').all(label_ok) {
        return Err(FieldError::new(
            field,
            if min_labels > 1 {
                "must be a domain name such as example.com: ASCII letters, digits and hyphens \
                 in dot-separated labels"
            } else {
                "must be ASCII letters, digits and hyphens, in dot-separated labels"
            },
        ));
    }
    Ok(v)
}

/// The name a `From:` header will show. It reaches a mail header, so a line break in it is
/// header injection and every control character is refused.
fn display_name(v: &str) -> Result<String, FieldError> {
    let v = v.trim();
    if v.is_empty() || v.chars().count() > 100 || v.chars().any(char::is_control) {
        return Err(FieldError::new(
            "from_name",
            "must be 1 to 100 characters with no control characters; send null to clear",
        ));
    }
    Ok(v.to_owned())
}

/// The DER inside a PEM private key. This unwraps the armour and nothing more: that the bytes are
/// a key this product can sign with is for the DKIM signer to say, not this route.
///
/// The refusal never echoes the input, which is a key when it is anything.
fn pem_to_der(v: &str) -> Result<Zeroizing<Vec<u8>>, FieldError> {
    const WHAT: &str = "must be an unencrypted RSA private key in PEM form (BEGIN PRIVATE KEY or \
                        BEGIN RSA PRIVATE KEY)";
    let refuse = || FieldError::new("dkim_private_key", WHAT);
    let lines: Vec<&str> = v.trim().lines().map(str::trim).collect();
    let [begin, body @ .., end] = lines.as_slice() else {
        return Err(refuse());
    };
    let label = begin
        .strip_prefix("-----BEGIN ")
        .and_then(|l| l.strip_suffix("-----"))
        .filter(|l| matches!(*l, "PRIVATE KEY" | "RSA PRIVATE KEY"))
        .ok_or_else(refuse)?;
    if *end != format!("-----END {label}-----") || v.len() > 16 * 1024 {
        return Err(refuse());
    }
    let der = Zeroizing::new(STANDARD.decode(body.concat()).map_err(|_| refuse())?);
    if der.is_empty() {
        return Err(refuse());
    }
    Ok(der)
}

/// The two refusals the table's constraints make, as the API's own.
fn write_error(e: sqlx::Error) -> ApiError {
    match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("sending-domain-taken"),
        Some(d) if d.constraint() == Some("sending_domains_app_signing_has_key") => {
            ApiError::Validation(vec![FieldError::new(
                "dkim_private_key",
                "is required while signing is app",
            )])
        }
        _ => e.into(),
    }
}

/// One row, with the columns the view and the checker both need.
struct Row {
    id: Uuid,
    workspace_id: Uuid,
    domain: String,
    from_name: Option<String>,
    signing: Signing,
    dkim_selector: String,
    has_dkim_key: bool,
    return_path_domain: Option<String>,
    checks: SqlJson<DnsChecks>,
    checked_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

impl Row {
    fn domain(&self) -> SendingDomain {
        SendingDomain {
            id: self.id,
            workspace_id: self.workspace_id,
            domain: self.domain.clone(),
            from_name: self.from_name.clone(),
            signing: self.signing,
            dkim_selector: self.dkim_selector.clone(),
            return_path_domain: self.return_path_domain.clone(),
            checks: self.checks.0.clone(),
            checked_at: self.checked_at,
        }
    }
}

impl From<Row> for SendingDomainView {
    fn from(r: Row) -> SendingDomainView {
        SendingDomainView {
            id: r.id,
            workspace_id: r.workspace_id,
            domain: r.domain,
            from_name: r.from_name,
            signing: r.signing,
            dkim_selector: r.dkim_selector,
            has_dkim_key: r.has_dkim_key,
            return_path_domain: r.return_path_domain,
            checks: r.checks.0,
            checked_at: r.checked_at,
        }
    }
}

async fn load(db: impl PgExecutor<'_>, workspace_id: Uuid, id: Uuid) -> ApiResult<Row> {
    Ok(sqlx::query_as!(
        Row,
        r#"select id, workspace_id, domain, from_name, signing as "signing: Signing",
                  dkim_selector, dkim_private_key_encrypted is not null as "has_dkim_key!",
                  return_path_domain, checks as "checks: SqlJson<DnsChecks>", checked_at,
                  updated_at
           from sending_domains where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await?)
}

/// Names the domain. Never the key: the audit log is read by admins and kept.
async fn record(
    db: &mut PgConnection,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (workspace_id, id): (Uuid, Uuid),
    metadata: serde_json::Value,
) -> ApiResult<()> {
    audit::record(
        db,
        meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action,
            target_type: Some("sending_domain"),
            target_id: Some(id.to_string()),
            metadata,
        },
    )
    .await?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/sending-domains",
    operation_id = "list_sending_domains",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first. Never carries a key.", body = Page<SendingDomainView>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_sending_domains(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<SendingDomainView>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Row,
        r#"select id, workspace_id, domain, from_name, signing as "signing: Signing",
                  dkim_selector, dkim_private_key_encrypted is not null as "has_dkim_key!",
                  return_path_domain, checks as "checks: SqlJson<DnsChecks>", checked_at,
                  updated_at
           from sending_domains
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    let views = rows.into_iter().map(SendingDomainView::from).collect();
    Ok(Json(Page::from_rows(views, limit, |r| r.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/sending-domains/{id}",
    operation_id = "get_sending_domain",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id"),
    ),
    responses(
        (status = 200, body = SendingDomainView),
        (status = 404, description = "not a member, or no such sending domain in this workspace"),
    ),
)]
pub async fn get_sending_domain(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<SendingDomainView>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(load(&mut *conn, workspace_id, id).await?.into()))
}

/// Which addresses a workspace may send as, and with what key, so writing it takes admin.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/sending-domains",
    operation_id = "create_sending_domain",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewSendingDomain,
    responses(
        (status = 201, body = SendingDomainView),
        (status = 403, description = "insufficient-role: configuring a sending domain takes admin"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "sending-domain-taken"),
        (status = 422, description = "an invalid or missing field, an app-signed domain without a \
            key, or an unknown field"),
    ),
)]
pub async fn create_sending_domain(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewSendingDomain>,
) -> ApiResult<(StatusCode, Json<SendingDomainView>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let mut errors = Vec::new();
    let domain = keep(dns_name("domain", &input.domain, 2), &mut errors);
    let selector = keep(
        dns_name(
            "dkim_selector",
            input.dkim_selector.as_deref().unwrap_or("nm1"),
            1,
        ),
        &mut errors,
    );
    let return_path = input
        .return_path_domain
        .as_deref()
        .and_then(|v| keep(dns_name("return_path_domain", v, 2), &mut errors));
    let from_name = input
        .from_name
        .as_deref()
        .and_then(|v| keep(display_name(v), &mut errors));
    let key = input
        .dkim_private_key
        .as_deref()
        .and_then(|v| keep(pem_to_der(v), &mut errors));
    if input.signing == Signing::App && input.dkim_private_key.is_none() {
        errors.push(FieldError::new(
            "dkim_private_key",
            "is required while signing is app",
        ));
    }
    let (Some(domain), Some(selector), true) = (domain, selector, errors.is_empty()) else {
        return Err(ApiError::Validation(errors));
    };
    let id = Uuid::now_v7();
    let encrypted = key.as_deref().map(|k| state.secrets.encrypt(k));
    sqlx::query!(
        "insert into sending_domains (id, workspace_id, domain, from_name, signing, \
         dkim_selector, dkim_private_key_encrypted, return_path_domain) \
         values ($1, $2, $3, $4, $5, $6, $7, $8)",
        id,
        workspace_id,
        domain,
        from_name,
        input.signing as Signing,
        selector,
        encrypted,
        return_path,
    )
    .execute(&mut *tx)
    .await
    .map_err(write_error)?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::SENDING_DOMAIN_CREATED,
        (workspace_id, id),
        json!({ "domain": domain, "signing": input.signing }),
    )
    .await?;
    let created = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/sending-domains/{id}",
    operation_id = "update_sending_domain",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id"),
    ),
    request_body = UpdateSendingDomain,
    responses(
        (status = 200, body = SendingDomainView),
        (status = 403, description = "insufficient-role: configuring a sending domain takes admin"),
        (status = 404, description = "not a member, or no such sending domain in this workspace"),
        (status = 422, description = "an invalid field, an explicit null for a field that cannot \
            be cleared, a change that leaves an app-signed domain without a key, or an unknown \
            field"),
    ),
)]
pub async fn update_sending_domain(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateSendingDomain>,
) -> ApiResult<Json<SendingDomainView>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    // Which keys were sent, by name. `dkim_private_key` here is the key and says that it changed,
    // never what it changed to.
    let fields: Vec<&str> = [
        ("from_name", input.from_name.is_some()),
        ("signing", input.signing.is_some()),
        ("dkim_selector", input.dkim_selector.is_some()),
        ("return_path_domain", input.return_path_domain.is_some()),
        ("dkim_private_key", input.dkim_private_key.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(field, _)| field)
    .collect();
    let UpdateSendingDomain {
        from_name: new_from_name,
        signing,
        dkim_selector,
        return_path_domain,
        dkim_private_key,
    } = input;
    // What `checks` described is gone when any of these is sent. `signing` is one: it changes
    // whose key has to be published at the selector, so a `dkim: pass` earned under the other mode
    // would tell an operator a configuration is verified that nothing has checked. Sent again with
    // the same value resets too: cheaper than reading the row to find out, and a re-check is one
    // click.
    let reset_checks = signing.is_some()
        || dkim_selector.is_some()
        || return_path_domain.is_some()
        || dkim_private_key.is_some();
    let mut errors = Vec::new();
    let from_name_sent = new_from_name.is_some();
    let new_from_name = new_from_name
        .as_ref()
        .and_then(|v| v.as_deref())
        .and_then(|v| keep(display_name(v), &mut errors));
    let signing = not_null("signing", signing, &mut errors);
    let dkim_selector = not_null("dkim_selector", dkim_selector, &mut errors)
        .and_then(|v| keep(dns_name("dkim_selector", &v, 1), &mut errors));
    let new_return_path = return_path_domain
        .as_ref()
        .and_then(|v| v.as_deref())
        .and_then(|v| keep(dns_name("return_path_domain", v, 2), &mut errors));
    let key = dkim_private_key
        .as_ref()
        .and_then(|v| v.as_deref())
        .and_then(|v| keep(pem_to_der(v), &mut errors));
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    let encrypted = key.as_deref().map(|k| state.secrets.encrypt(k));
    // `$3`, `$7`, `$9` and `$11` say whether the key was sent; the value beside each is what to
    // store, and `null` there is what clears. The table's own check refuses a change that leaves
    // an app-signed domain without a key, and `write_error` turns that into the field error.
    let changed = sqlx::query!(
        "update sending_domains set \
         from_name = case when $3 then $4 else from_name end, \
         signing = coalesce($5, signing), \
         dkim_selector = coalesce($6, dkim_selector), \
         return_path_domain = case when $7 then $8 else return_path_domain end, \
         dkim_private_key_encrypted = case when $9 then $10 else dkim_private_key_encrypted end, \
         checks = case when $11 then '{}'::jsonb else checks end, \
         checked_at = case when $11 then null else checked_at end, \
         updated_at = now() \
         where workspace_id = $1 and id = $2 returning domain",
        workspace_id,
        id,
        from_name_sent,
        new_from_name,
        signing as Option<Signing>,
        dkim_selector,
        return_path_domain.is_some(),
        new_return_path,
        dkim_private_key.is_some(),
        encrypted,
        reset_checks,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(write_error)?
    .ok_or(ApiError::NotFound)?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::SENDING_DOMAIN_UPDATED,
        (workspace_id, id),
        json!({ "domain": changed.domain, "fields": fields }),
    )
    .await?;
    let updated = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(updated.into()))
}

/// A hard delete.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/sending-domains/{id}",
    operation_id = "delete_sending_domain",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "insufficient-role: configuring a sending domain takes admin"),
        (status = 404, description = "not a member, or no such sending domain in this workspace"),
    ),
)]
pub async fn delete_sending_domain(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let gone = sqlx::query!(
        "delete from sending_domains where workspace_id = $1 and id = $2 returning domain",
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::SENDING_DOMAIN_DELETED,
        (workspace_id, id),
        json!({ "domain": gone.domain }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Looks the domain's records up, stores what it finds as `checks` and returns the domain.
///
/// **Holds no database connection while it waits on DNS**: the row is read, the connection goes
/// back to the pool, the lookups run, and a second statement writes the result. A nameserver that
/// hangs costs this request five seconds and the pool nothing.
///
/// Not audited: the result is derived from DNS, not something an operator configured, and the
/// route is limited to a few a minute.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/sending-domains/{id}/check",
    operation_id = "check_sending_domain",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Sending domain id"),
    ),
    responses(
        (status = 200, description = "The domain with its `checks` and `checked_at` just written. \
            A failing check is a result, not an error.", body = SendingDomainView),
        (status = 403, description = "insufficient-role: checking a sending domain takes admin"),
        (status = 404, description = "not a member, or no such sending domain in this workspace"),
        (status = 409, description = "sending-domain-changed: it was edited while being checked"),
        (status = 429, description = "rate-limited: too many checks for this workspace"),
    ),
)]
pub async fn check_sending_domain(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    authorize(&state.pool, &principal, workspace_id, Role::Admin).await?;
    let quota = state.check_limit.check(workspace_id);
    if let Some(retry_after_secs) = quota.over {
        return Ok((
            quota.headers,
            ApiError::TooManyRequests { retry_after_secs },
        )
            .into_response());
    }
    let row = load(&state.pool, workspace_id, id).await?;
    // `expected_p` is `None` until the DKIM signer exists to derive it from the stored key.
    let checks = dns::check(&state.resolver, &row.domain(), None).await;
    // Only if the configuration is the one that was checked: an edit that landed during the
    // lookups has reset `checks`, and writing the old answer over that would put a verdict on a
    // selector nobody uses any more.
    let stored = sqlx::query!(
        "update sending_domains set checks = $4, checked_at = now() \
         where workspace_id = $1 and id = $2 and updated_at = $3 returning id",
        workspace_id,
        id,
        row.updated_at,
        SqlJson(&checks) as SqlJson<&DnsChecks>,
    )
    .fetch_optional(&state.pool)
    .await?;
    if stored.is_none() {
        // Gone (404 from the read) or edited (409).
        load(&state.pool, workspace_id, id).await?;
        return Err(ApiError::Conflict("sending-domain-changed"));
    }
    let view: SendingDomainView = load(&state.pool, workspace_id, id).await?.into();
    Ok((quota.headers, Json(view)).into_response())
}
