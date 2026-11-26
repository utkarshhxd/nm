// SPDX-License-Identifier: AGPL-3.0-only

//! The campaign API (contract: `.superpowers/sdd/m2c/contract.md`; spec M2c §4.1, §4.3, §4.8).
//!
//! **Roles** (spec §4.1): viewers read, preview and estimate; editors draft, delete drafts,
//! test-send and pause; admins schedule, unschedule, resume, cancel and pick the winner.
//!
//! **States.** A transition into the state the campaign is already in answers 200 with the view;
//! any other wrong state is `409 campaign-state`. Every transition is one guarded `update … where
//! state = …`, so a retry or a race reads the answer from the row, not from a check made earlier.
//!
//! **Locks** (ADR 0005, M2c table): every write here is one statement on the `campaigns` row, and a
//! `jobs` insert after it where a job is queued, so `campaigns` comes before `jobs`. None of them
//! writes `workspace_id`, so none takes anything on `workspaces` except `create_campaign`'s insert.
//! `prepare` and every `mrml` run happen before the transaction opens.

use std::{collections::HashSet, str::FromStr};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{NaiveDateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder, types::Json as SqlJson};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{
    Campaign, CampaignCounts, CampaignEstimate, CampaignPreview, CampaignSummary, CampaignView,
    Frozen, LOCAL_FORMAT, MAX_VARIANTS, Mode, Refusal, State as CampaignState, Variant, load,
    load_frozen, prepare, schedule,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::{Definition, definitions_for},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::{
        rate_limit::{MAX_TEST_ADDRESSES, RateLimiter},
        request_meta::RequestMeta,
    },
    pagination::{Page, PageParams},
    segments::{self, compile::push_rules},
    template::{
        render::Contact,
        routes::sample,
        tag::Registry,
        text::{compile_subject, render_subject},
    },
    users::normalize_email,
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    use axum::routing::{get, post};
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns",
            get(list_campaigns).post(create_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}",
            get(get_campaign)
                .patch(update_campaign)
                .delete(delete_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/preview",
            post(preview_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/estimate",
            post(estimate_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/test",
            post(test_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/schedule",
            post(schedule_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/unschedule",
            post(unschedule_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/pause",
            post(pause_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/resume",
            post(resume_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/cancel",
            post(cancel_campaign),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/campaigns/{id}/winner",
            post(choose_winner),
        )
}

// ---------------------------------------------------------------------------------------------
// Wire types

/// `?limit=&cursor=`; the contract's `after` is accepted as the same thing.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListParams {
    /// Page size, 1 to 200 (default 50).
    pub limit: Option<u32>,
    /// Opaque cursor from the previous page's `next_cursor`.
    #[serde(alias = "after")]
    pub cursor: Option<String>,
}

/// A campaign to create. Everything but `name` may be filled in later.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewCampaign {
    pub name: String,
    pub list_ids: Option<Vec<Uuid>>,
    pub segment_id: Option<Uuid>,
    pub sending_domain_id: Option<Uuid>,
    pub from_local_part: Option<String>,
    pub from_name: Option<String>,
    pub reply_to: Option<String>,
    pub variants: Option<Vec<Variant>>,
    pub ab_test_percent: Option<i64>,
}

/// A key left out is left alone; `null` clears a nullable one.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateCampaign {
    pub name: Option<String>,
    pub list_ids: Option<Vec<Uuid>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Uuid>)]
    pub segment_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Uuid>)]
    pub sending_domain_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub from_local_part: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub from_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub reply_to: Option<Option<String>>,
    pub variants: Option<Vec<Variant>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<i64>)]
    pub ab_test_percent: Option<Option<i64>>,
}

/// Tells a key that is absent from one present as `null`. The helper every patch body carries.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    #[schema(value_type = Mode)]
    pub mode: String,
    /// An IANA zone name.
    pub zone: String,
    /// `YYYY-MM-DDTHH:MM`, no zone. Null sends now.
    pub local: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestSendRequest {
    /// 1 to 5 addresses.
    pub addresses: Vec<String>,
    /// Which variant to send; default 0.
    pub variant: Option<usize>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TestSendResult {
    pub queued: usize,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WinnerRequest {
    pub variant: usize,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CampaignPreviewRequest {
    pub variant: Option<usize>,
}

// ---------------------------------------------------------------------------------------------
// Helpers

fn campaign_state(state: CampaignState) -> ApiError {
    ApiError::ConflictDetail {
        code: "campaign-state",
        detail: format!(
            "the campaign is {}, which does not allow this",
            state.as_str()
        ),
    }
}

fn invalid(field: &str, message: &str) -> ApiError {
    ApiError::Validation(vec![FieldError::new(field, message)])
}

/// A `prepare` refusal as the contract's problem code.
fn refused(r: Refusal) -> ApiError {
    let detail = r.to_string();
    match r {
        Refusal::PostalAddress => ApiError::ConflictDetail {
            code: "postal-address-required",
            detail,
        },
        Refusal::DomainUnverified(_) => ApiError::ConflictDetail {
            code: "sending-domain-unverified",
            detail,
        },
        Refusal::Drift { .. } => ApiError::ConflictDetail {
            code: "template-stale",
            detail,
        },
        Refusal::Missing { field, what } => invalid(&field, &format!("needs {what}")),
        Refusal::Template { variant, error } => {
            invalid(&format!("variants[{variant}].template_id"), &error)
        }
        Refusal::Subject { variant, error } => {
            invalid(&format!("variants[{variant}].subject"), &error)
        }
        Refusal::Internal(e) => ApiError::Internal(anyhow::anyhow!(e)),
    }
}

async fn record(
    db: &mut PgConnection,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (workspace_id, id): (Uuid, Uuid),
    metadata: Value,
) -> ApiResult<()> {
    audit::record(
        db,
        meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action,
            target_type: Some("campaign"),
            target_id: Some(id.to_string()),
            metadata,
        },
    )
    .await?;
    Ok(())
}

/// Every answer carries the `RateLimit` headers, refusals included.
async fn limited<T: IntoResponse>(
    limiter: &RateLimiter,
    workspace_id: Uuid,
    work: impl Future<Output = ApiResult<T>>,
) -> Response {
    let quota = limiter.check(workspace_id);
    if let Some(retry_after_secs) = quota.over {
        return (
            quota.headers,
            ApiError::TooManyRequests { retry_after_secs },
        )
            .into_response();
    }
    match work.await {
        Ok(v) => (quota.headers, v).into_response(),
        Err(e) => (quota.headers, e).into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// Validation at save (no freeze here: that is `prepare`'s)

/// The editable fields, merged and normalised.
struct Fields {
    list_ids: Vec<Uuid>,
    segment_id: Option<Uuid>,
    sending_domain_id: Option<Uuid>,
    from_local_part: Option<String>,
    from_name: Option<String>,
    reply_to: Option<String>,
    variants: Vec<Variant>,
    ab_test_percent: Option<i64>,
}

impl Fields {
    fn of(c: &Campaign) -> Fields {
        Fields {
            list_ids: c.list_ids.clone(),
            segment_id: c.segment_id,
            sending_domain_id: c.sending_domain_id,
            from_local_part: c.from_local_part.clone(),
            from_name: c.from_name.clone(),
            reply_to: c.reply_to.clone(),
            variants: c.variants.0.clone(),
            ab_test_percent: c.ab_test_percent.map(i64::from),
        }
    }
}

/// RFC 5322 `dot-atom`: atext runs joined by single dots. Held to 64 bytes, the SMTP limit.
fn dot_atom(s: &str) -> bool {
    const ATEXT: &[u8] = b"!#$%&'*+-/=?^_`{|}~";
    s.len() <= 64
        && s.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || ATEXT.contains(&b))
        })
}

fn address_ok(s: &str) -> bool {
    s.len() <= 254 && s.is_ascii() && lettre::Address::from_str(s).is_ok()
}

fn display_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    ((1..=100).contains(&name.chars().count()) && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}

/// Checks `f` against the workspace and trims what it keeps, returning every refusal at once.
/// Plain selects only: nothing here locks a row.
async fn check(
    db: &mut PgConnection,
    ws: Uuid,
    defs: &[Definition],
    f: &mut Fields,
) -> ApiResult<Vec<FieldError>> {
    let mut errors = Vec::new();

    // Audience.
    let mut seen = HashSet::new();
    f.list_ids.retain(|id| seen.insert(*id));
    if f.list_ids.len() > 100 {
        errors.push(FieldError::new("list_ids", "at most 100 lists"));
    } else if !f.list_ids.is_empty() {
        let found: Vec<Uuid> =
            sqlx::query_scalar("select id from lists where workspace_id = $1 and id = any($2)")
                .bind(ws)
                .bind(&f.list_ids)
                .fetch_all(&mut *db)
                .await?;
        if found.len() != f.list_ids.len() {
            errors.push(FieldError::new(
                "list_ids",
                "names a list that is not in this workspace",
            ));
        }
    }
    if let Some(id) = f.segment_id {
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from segments where workspace_id = $1 and id = $2)",
        )
        .bind(ws)
        .bind(id)
        .fetch_one(&mut *db)
        .await?;
        if !found {
            errors.push(FieldError::new(
                "segment_id",
                "is not a segment in this workspace",
            ));
        }
    }

    // Sender.
    if let Some(id) = f.sending_domain_id {
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from sending_domains where workspace_id = $1 and id = $2)",
        )
        .bind(ws)
        .bind(id)
        .fetch_one(&mut *db)
        .await?;
        if !found {
            errors.push(FieldError::new(
                "sending_domain_id",
                "is not a sending domain in this workspace",
            ));
        }
    }
    if let Some(local) = &f.from_local_part
        && !(dot_atom(local) && address_ok(&format!("{local}@example.com")))
    {
        errors.push(FieldError::new(
            "from_local_part",
            "must be the part of an address before the @: letters, digits and . _ + - with no \
             leading, trailing or doubled dot",
        ));
    }
    if let Some(name) = &f.from_name {
        match display_name(name) {
            Some(n) => f.from_name = Some(n),
            None => errors.push(FieldError::new(
                "from_name",
                "must be 1-100 characters with no control characters",
            )),
        }
    }
    if let Some(r) = &f.reply_to {
        let r = r.trim().to_owned();
        if !address_ok(&r) {
            errors.push(FieldError::new("reply_to", "must be an email address"));
        }
        f.reply_to = Some(r);
    }

    // Variants.
    if f.variants.len() > MAX_VARIANTS {
        errors.push(FieldError::new(
            "variants",
            format!("at most {MAX_VARIANTS} variants"),
        ));
    }
    let registry = Registry::from_definitions(defs);
    let ids: Vec<Uuid> = f.variants.iter().map(|v| v.template_id).collect();
    let templates: HashSet<Uuid> =
        sqlx::query_scalar("select id from templates where workspace_id = $1 and id = any($2)")
            .bind(ws)
            .bind(&ids)
            .fetch_all(&mut *db)
            .await?
            .into_iter()
            .collect();
    for (i, v) in f.variants.iter_mut().enumerate() {
        let subject = v.subject.trim().to_owned();
        if subject.is_empty() || subject.chars().count() > 300 {
            errors.push(FieldError::new(
                format!("variants[{i}].subject"),
                "must be 1-300 characters",
            ));
        } else if subject.chars().any(char::is_control) {
            errors.push(FieldError::new(
                format!("variants[{i}].subject"),
                "cannot hold a line break or any other control character",
            ));
        } else if let Err(e) = compile_subject(&subject, &registry) {
            errors.push(FieldError::new(
                format!("variants[{i}].subject"),
                crate::template::mjml::CompileError::Tag(e).to_string(),
            ));
        }
        v.subject = subject;
        if !templates.contains(&v.template_id) {
            errors.push(FieldError::new(
                format!("variants[{i}].template_id"),
                "is not a template in this workspace",
            ));
        }
        if let Some(name) = &v.from_name {
            match display_name(name) {
                Some(n) => v.from_name = Some(n),
                None => errors.push(FieldError::new(
                    format!("variants[{i}].from_name"),
                    "must be 1-100 characters with no control characters",
                )),
            }
        }
    }

    // A/B.
    match (f.ab_test_percent, f.variants.len() > 1) {
        (Some(p), true) if !(10..=100).contains(&p) => errors.push(FieldError::new(
            "ab_test_percent",
            "must be between 10 and 100",
        )),
        (Some(_), false) => errors.push(FieldError::new(
            "ab_test_percent",
            "applies only with two or more variants",
        )),
        _ => {}
    }
    Ok(errors)
}

/// The columns a validated [`Fields`] writes.
fn percent(f: &Fields) -> Option<i16> {
    f.ab_test_percent.and_then(|p| i16::try_from(p).ok())
}

// ---------------------------------------------------------------------------------------------
// Read

#[derive(sqlx::FromRow)]
struct SummaryRow {
    id: Uuid,
    name: String,
    state: CampaignState,
    send_at: Option<chrono::DateTime<Utc>>,
    total: i64,
    sent: i64,
    failed: i64,
    unknown: i64,
    skipped: i64,
    updated_at: chrono::DateTime<Utc>,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/campaigns",
    operation_id = "list_campaigns",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), ListParams),
    responses(
        (status = 200, description = "Newest first", body = Page<CampaignSummary>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_campaigns(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(params): ApiQuery<ListParams>,
) -> ApiResult<Json<Page<CampaignSummary>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let page = PageParams {
        limit: params.limit,
        cursor: params.cursor,
    };
    let limit = page.limit();
    let rows = sqlx::query_as::<_, SummaryRow>(
        "select id, name, state, send_at, total, sent, failed, unknown, skipped, updated_at \
         from campaigns where workspace_id = $1 and ($2::uuid is null or id < $2) \
         order by id desc limit $3",
    )
    .bind(workspace_id)
    .bind(page.after()?)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let rows = rows
        .into_iter()
        .map(|r| CampaignSummary {
            id: r.id,
            name: r.name,
            state: r.state,
            send_at: r.send_at,
            counts: CampaignCounts::new(r.total, r.sent, r.failed, r.unknown, r.skipped),
            updated_at: r.updated_at,
        })
        .collect();
    Ok(Json(Page::from_rows(rows, limit, |c| c.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}",
    operation_id = "get_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 200, body = CampaignView),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
    ),
)]
pub async fn get_campaign(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<CampaignView>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(load(&state.pool, workspace_id, id).await?.view()))
}

// ---------------------------------------------------------------------------------------------
// Draft: create, edit, delete

const INSERT: &str = "insert into campaigns (id, workspace_id, name, list_ids, segment_id, \
    sending_domain_id, from_local_part, from_name, reply_to, variants, ab_test_percent) \
    values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)";

const UPDATE: &str = "update campaigns set name = $3, list_ids = $4, segment_id = $5, \
    sending_domain_id = $6, from_local_part = $7, from_name = $8, reply_to = $9, variants = $10, \
    ab_test_percent = $11, updated_at = now() \
    where workspace_id = $1 and id = $2 and state = 'draft'";

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns",
    operation_id = "create_campaign",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewCampaign,
    responses(
        (status = 201, body = CampaignView),
        (status = 403, description = "insufficient-role: drafting takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a name, list, segment, domain, template, from address, \
            reply-to, subject or A/B percentage that is not valid"),
    ),
)]
pub async fn create_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewCampaign>,
) -> ApiResult<(StatusCode, Json<CampaignView>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let mut errors = Vec::new();
    let name = match crate::lists::validate_name(&input.name) {
        Ok(n) => n,
        Err(e) => {
            errors.push(e);
            String::new()
        }
    };
    if input.variants.as_ref().is_some_and(Vec::is_empty) {
        errors.push(FieldError::new("variants", "needs at least one variant"));
    }
    let mut f = Fields {
        list_ids: input.list_ids.unwrap_or_default(),
        segment_id: input.segment_id,
        sending_domain_id: input.sending_domain_id,
        from_local_part: input.from_local_part,
        from_name: input.from_name,
        reply_to: input.reply_to,
        variants: input.variants.unwrap_or_default(),
        ab_test_percent: input.ab_test_percent,
    };
    let defs = definitions_for(&mut *tx, workspace_id).await?;
    errors.extend(check(&mut tx, workspace_id, &defs, &mut f).await?);
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    let id = Uuid::now_v7();
    // ONE locking statement (ADR 0005): the insert takes `for key share` on the workspace row.
    sqlx::query(INSERT)
        .bind(id)
        .bind(workspace_id)
        .bind(&name)
        .bind(&f.list_ids)
        .bind(f.segment_id)
        .bind(f.sending_domain_id)
        .bind(&f.from_local_part)
        .bind(&f.from_name)
        .bind(&f.reply_to)
        .bind(SqlJson(&f.variants))
        .bind(percent(&f))
        .execute(&mut *tx)
        .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::CAMPAIGN_CREATED,
        (workspace_id, id),
        json!({ "name": name }),
    )
    .await?;
    let created = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(created.view())))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}",
    operation_id = "update_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    request_body = UpdateCampaign,
    responses(
        (status = 200, body = CampaignView),
        (status = 403, description = "insufficient-role: drafting takes editor"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: only a draft's content is editable"),
        (status = 422, description = "a value that is not valid"),
    ),
)]
pub async fn update_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateCampaign>,
) -> ApiResult<Json<CampaignView>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    // A plain select, no `for update`: the guarded update below is the one locking statement.
    let row = load(&mut *tx, workspace_id, id).await?;
    if row.state != CampaignState::Draft {
        return Err(campaign_state(row.state));
    }
    let mut errors = Vec::new();
    let name = match input.name.as_deref() {
        Some(n) => crate::lists::validate_name(n).unwrap_or_else(|e| {
            errors.push(e);
            String::new()
        }),
        None => row.name.clone(),
    };
    if input.variants.as_ref().is_some_and(Vec::is_empty) {
        errors.push(FieldError::new("variants", "needs at least one variant"));
    }
    let mut f = Fields::of(&row);
    let mut sent = Vec::new();
    macro_rules! apply {
        ($key:ident) => {
            if let Some(v) = input.$key {
                f.$key = v;
                sent.push(stringify!($key));
            }
        };
    }
    apply!(segment_id);
    apply!(sending_domain_id);
    apply!(from_local_part);
    apply!(from_name);
    apply!(reply_to);
    apply!(ab_test_percent);
    if let Some(v) = input.list_ids {
        f.list_ids = v;
        sent.push("list_ids");
    }
    if let Some(v) = input.variants {
        f.variants = v;
        sent.push("variants");
        // Going down to one variant ends the test, unless the request names a percentage.
        if f.variants.len() <= 1 && !sent.contains(&"ab_test_percent") {
            f.ab_test_percent = None;
        }
    }
    let defs = definitions_for(&mut *tx, workspace_id).await?;
    errors.extend(check(&mut tx, workspace_id, &defs, &mut f).await?);
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    // One locking statement; `workspace_id` is in the `where`, never the `set`, so it takes
    // nothing on `workspaces` (ADR 0005). The `state` guard is what makes the race a 409.
    let n = sqlx::query(UPDATE)
        .bind(workspace_id)
        .bind(id)
        .bind(&name)
        .bind(&f.list_ids)
        .bind(f.segment_id)
        .bind(f.sending_domain_id)
        .bind(&f.from_local_part)
        .bind(&f.from_name)
        .bind(&f.reply_to)
        .bind(SqlJson(&f.variants))
        .bind(percent(&f))
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(campaign_state(
            load(&mut *tx, workspace_id, id).await?.state,
        ));
    }
    record(
        &mut tx,
        &meta,
        &principal,
        actions::CAMPAIGN_UPDATED,
        (workspace_id, id),
        json!({ "name": name, "fields": sent }),
    )
    .await?;
    let updated = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(updated.view()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}",
    operation_id = "delete_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "insufficient-role: deleting a draft takes editor"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: only a draft is deletable"),
    ),
)]
pub async fn delete_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    // `state = 'draft'` cannot match a campaign that has messages or a live launch job, which is
    // what lets `messages -> campaigns` stay `no action` (ADR 0005). Its test jobs cascade.
    let gone: Option<String> = sqlx::query_scalar(
        "delete from campaigns where workspace_id = $1 and id = $2 and state = 'draft' \
         returning name",
    )
    .bind(workspace_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(name) = gone else {
        return Err(campaign_state(
            load(&mut *tx, workspace_id, id).await?.state,
        ));
    };
    record(
        &mut tx,
        &meta,
        &principal,
        actions::CAMPAIGN_DELETED,
        (workspace_id, id),
        json!({ "name": name }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Transitions

/// What a guarded transition does. Each statement matches only the states the machine allows it
/// from, and writes no foreign key column.
struct Move {
    sql: &'static str,
    to: CampaignState,
    role: Role,
    action: &'static str,
}

const UNSCHEDULE: Move = Move {
    sql: "update campaigns set state = 'draft', launch_job_id = null, send_at = null, \
          schedule_mode = 'instant', schedule_zone = null, schedule_local = null, \
          updated_at = now() where workspace_id = $1 and id = $2 and state = 'scheduled'",
    to: CampaignState::Draft,
    role: Role::Admin,
    action: actions::CAMPAIGN_UNSCHEDULED,
};

const PAUSE: Move = Move {
    sql: "update campaigns set state = 'paused', updated_at = now() \
          where workspace_id = $1 and id = $2 and state = 'sending'",
    to: CampaignState::Paused,
    role: Role::Editor,
    action: actions::CAMPAIGN_PAUSED,
};

const CANCEL: Move = Move {
    sql: "update campaigns set state = 'cancelled', finished_at = coalesce(finished_at, now()), \
          updated_at = now() \
          where workspace_id = $1 and id = $2 and state in ('scheduled', 'sending', 'paused')",
    to: CampaignState::Cancelled,
    role: Role::Admin,
    action: actions::CAMPAIGN_CANCELLED,
};

/// One guarded update and its audit row. When the guard matches nothing the answer is read from the
/// row: already in the target state is a 200, anything else a 409.
async fn transition(
    state: &AppState,
    meta: &RequestMeta,
    principal: &Principal,
    (workspace_id, id): (Uuid, Uuid),
    mv: Move,
) -> ApiResult<Json<CampaignView>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, mv.role).await?;
    let n = sqlx::query(mv.sql)
        .bind(workspace_id)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 1 {
        record(
            &mut tx,
            meta,
            principal,
            mv.action,
            (workspace_id, id),
            json!({ "to": mv.to.as_str() }),
        )
        .await?;
    }
    let row = load(&mut *tx, workspace_id, id).await?;
    if n == 0 && row.state != mv.to {
        return Err(campaign_state(row.state));
    }
    tx.commit().await?;
    Ok(Json(row.view()))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/unschedule",
    operation_id = "unschedule_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 200, description = "The campaign is a draft again", body = CampaignView),
        (status = 403, description = "insufficient-role: unscheduling takes admin"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: only a scheduled campaign is unscheduled"),
    ),
)]
pub async fn unschedule_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<CampaignView>> {
    transition(&state, &meta, &principal, (workspace_id, id), UNSCHEDULE).await
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/pause",
    operation_id = "pause_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 200, description = "The campaign is paused", body = CampaignView),
        (status = 403, description = "insufficient-role: pausing takes editor"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: only a sending campaign is paused"),
    ),
)]
pub async fn pause_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<CampaignView>> {
    transition(&state, &meta, &principal, (workspace_id, id), PAUSE).await
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/cancel",
    operation_id = "cancel_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 200, description = "The campaign is cancelled", body = CampaignView),
        (status = 403, description = "insufficient-role: cancelling takes admin"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: a scheduled, sending or paused campaign is \
            cancelled; a sent or failed one is not"),
    ),
)]
pub async fn cancel_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<CampaignView>> {
    transition(&state, &meta, &principal, (workspace_id, id), CANCEL).await
}

// ---------------------------------------------------------------------------------------------
// Schedule

const SCHEDULE: &str = "update campaigns set state = 'scheduled', schedule_mode = $3, \
    schedule_zone = $4, schedule_local = $5, send_at = $6, launch_job_id = $7, updated_at = now() \
    where workspace_id = $1 and id = $2 and state = 'draft'";

/// The control job that resolves the audience (R5). `campaign_id` is a column, not payload.
const LAUNCH_JOB: &str = "insert into jobs (id, workspace_id, kind, campaign_id, payload, run_after) \
    values ($1, $2, 'campaign_launch', $3, '{}', $4)";

fn parse_local(raw: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(raw, LOCAL_FORMAT)
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/schedule",
    operation_id = "schedule_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    request_body = ScheduleRequest,
    responses(
        (status = 200, description = "Scheduled; `schedule.send_at` is the resolved instant", body = CampaignView),
        (status = 403, description = "insufficient-role: sending takes admin"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state; postal-address-required; \
            sending-domain-unverified; or template-stale"),
        (status = 422, description = "an unknown mode or zone, a local time that is not in the \
            future, recipient_local with more than one variant, or an incomplete campaign"),
    ),
)]
pub async fn schedule_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<ScheduleRequest>,
) -> ApiResult<Json<CampaignView>> {
    authorize(&state.pool, &principal, workspace_id, Role::Admin).await?;
    let campaign = load(&state.pool, workspace_id, id).await?;
    match campaign.state {
        CampaignState::Scheduled => return Ok(Json(campaign.view())),
        CampaignState::Draft => {}
        other => return Err(campaign_state(other)),
    }

    let mut errors = Vec::new();
    let mode = match input.mode.as_str() {
        "instant" => Some(Mode::Instant),
        "recipient_local" => Some(Mode::RecipientLocal),
        _ => {
            errors.push(FieldError::new(
                "schedule.mode",
                "must be instant or recipient_local",
            ));
            None
        }
    };
    let zone = input.zone.parse::<Tz>().ok();
    if zone.is_none() {
        errors.push(FieldError::new(
            "schedule.zone",
            "is not an IANA time zone name",
        ));
    }
    let local = match input.local.as_deref().map(parse_local) {
        None => None,
        Some(Some(l)) => Some(l),
        Some(None) => {
            errors.push(FieldError::new(
                "schedule.local",
                "must be a date and time like 2027-03-14T09:30",
            ));
            None
        }
    };
    if mode == Some(Mode::RecipientLocal) && campaign.variants.0.len() > 1 {
        errors.push(FieldError::new(
            "schedule.mode",
            "recipient_local cannot be used with more than one variant: a test cohort spread \
             over 26 hours has no meaningful winner",
        ));
    }
    let now = Utc::now();
    let send_at = match (mode, zone, local) {
        (_, _, None) => now,
        (Some(Mode::Instant), Some(zone), Some(l)) => schedule::resolve(l, zone),
        (Some(Mode::RecipientLocal), _, Some(l)) => schedule::earliest(l),
        _ => now,
    };
    if local.is_some() && errors.is_empty() && send_at <= now {
        errors.push(FieldError::new(
            "schedule.local",
            if mode == Some(Mode::RecipientLocal) {
                "has already passed in the earliest time zone, so some recipients' time is over"
            } else {
                "must be in the future"
            },
        ));
    }
    let (Some(mode), Some(zone), true) = (mode, zone, errors.is_empty()) else {
        return Err(ApiError::Validation(errors));
    };

    // The one validation, OUTSIDE the transaction: `mrml` runs here.
    prepare(&state.pool, &state.config.public_url, &campaign)
        .await
        .map_err(refused)?;

    let job = Uuid::now_v7();
    let mut tx = state.pool.begin().await?;
    // `campaigns`, then `jobs` (ADR 0005): the insert's foreign key takes `for key share` on this
    // same campaign and its workspace at the end of its own statement.
    let n = sqlx::query(SCHEDULE)
        .bind(workspace_id)
        .bind(id)
        .bind(mode)
        .bind(zone.name())
        .bind(local)
        .bind(send_at)
        .bind(job)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        // Lost a race: another request moved it first. Scheduled is the answer to a repeat.
        drop(tx);
        let row = load(&state.pool, workspace_id, id).await?;
        return match row.state {
            CampaignState::Scheduled => Ok(Json(row.view())),
            other => Err(campaign_state(other)),
        };
    }
    sqlx::query(LAUNCH_JOB)
        .bind(job)
        .bind(workspace_id)
        .bind(id)
        .bind(send_at)
        .execute(&mut *tx)
        .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::CAMPAIGN_SCHEDULED,
        (workspace_id, id),
        json!({ "mode": mode, "zone": zone.name(), "send_at": send_at }),
    )
    .await?;
    let scheduled = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(scheduled.view()))
}

// ---------------------------------------------------------------------------------------------
// Resume and winner

const RESUME: &str = "update campaigns set state = 'sending', pause_reason = null, \
    updated_at = now() where workspace_id = $1 and id = $2 and state = 'paused'";

const RESUME_REFROZEN: &str = "update campaigns set state = 'sending', pause_reason = null, \
    frozen = $3, frozen_at = now(), updated_at = now() \
    where workspace_id = $1 and id = $2 and state = 'paused'";

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/resume",
    operation_id = "resume_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 200, description = "The campaign is sending", body = CampaignView),
        (status = 403, description = "insufficient-role: resuming takes admin"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state; or, when the frozen rendition no longer \
            passes and a fresh one is refused, the same refusals as scheduling (the campaign \
            stays paused)"),
        (status = 422, description = "a fresh rendition is refused on a field"),
    ),
)]
pub async fn resume_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<CampaignView>> {
    authorize(&state.pool, &principal, workspace_id, Role::Admin).await?;
    let campaign = load(&state.pool, workspace_id, id).await?;
    match campaign.state {
        CampaignState::Sending => return Ok(Json(campaign.view())),
        CampaignState::Paused => {}
        other => return Err(campaign_state(other)),
    }
    // The frozen rendition is kept unless it no longer passes `ready` against the fields as they
    // are now (spec §4.7 ruling 10). Then it is built again, and a refusal is the answer.
    let defs = definitions_for(&state.pool, workspace_id).await?;
    let usable = matches!(
        load_frozen(&state.pool, workspace_id, id).await?,
        Some(Ok(f)) if frozen_usable(&f, &defs)
    );
    let refrozen: Option<Frozen> = if usable {
        None
    } else {
        Some(
            prepare(&state.pool, &state.config.public_url, &campaign)
                .await
                .map_err(refused)?,
        )
    };
    let mut tx = state.pool.begin().await?;
    let n = match &refrozen {
        None => {
            sqlx::query(RESUME)
                .bind(workspace_id)
                .bind(id)
                .execute(&mut *tx)
                .await?
        }
        Some(f) => {
            sqlx::query(RESUME_REFROZEN)
                .bind(workspace_id)
                .bind(id)
                .bind(serde_json::to_value(f).map_err(|e| anyhow::anyhow!(e))?)
                .execute(&mut *tx)
                .await?
        }
    }
    .rows_affected();
    if n == 1 && !campaign.resolved {
        // A system pause (or an admin's) may have ended the launch job, leaving the audience half
        // resolved with nothing to finish it. A new job takes over from the stored cursor, with the
        // phase's snapshot copied from the one it replaces. `campaigns`, then `jobs` (ADR 0005).
        let job = Uuid::now_v7();
        sqlx::query("update campaigns set launch_job_id = $3 where workspace_id = $1 and id = $2")
            .bind(workspace_id)
            .bind(id)
            .bind(job)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into jobs (id, workspace_id, kind, campaign_id, payload, run_after)
             values ($1, $2, 'campaign_launch', $3,
                     coalesce((select payload from jobs where id = $4 and workspace_id = $2),
                              '{}'::jsonb), now())",
        )
        .bind(job)
        .bind(workspace_id)
        .bind(id)
        .bind(campaign.launch_job_id)
        .execute(&mut *tx)
        .await?;
    }
    if n == 1 {
        record(
            &mut tx,
            &meta,
            &principal,
            actions::CAMPAIGN_RESUMED,
            (workspace_id, id),
            json!({ "refrozen": refrozen.is_some() }),
        )
        .await?;
    }
    let row = load(&mut *tx, workspace_id, id).await?;
    if n == 0 && row.state != CampaignState::Sending {
        return Err(campaign_state(row.state));
    }
    tx.commit().await?;
    Ok(Json(row.view()))
}

const WINNER: &str = "update campaigns set winner = $3, phase = 2, resolved = false, \
    resolve_cursor = null, launch_job_id = $4, updated_at = now() \
    where workspace_id = $1 and id = $2 and state in ('sending', 'paused') and phase = 1 \
    and resolved and ab_test_percent < 100";

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/winner",
    operation_id = "choose_campaign_winner",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    request_body = WinnerRequest,
    responses(
        (status = 200, description = "The winner is chosen and the remainder is queued to resolve", body = CampaignView),
        (status = 403, description = "insufficient-role: picking the winner takes admin"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: not sending or paused, no A/B test below \
            100%, the test cohort not fully resolved, or a winner already chosen"),
        (status = 422, description = "a variant index the campaign does not have"),
    ),
)]
pub async fn choose_winner(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<WinnerRequest>,
) -> ApiResult<Json<CampaignView>> {
    authorize(&state.pool, &principal, workspace_id, Role::Admin).await?;
    let campaign = load(&state.pool, workspace_id, id).await?;
    let variant = i16::try_from(input.variant).ok();
    if campaign.winner.is_some() && campaign.winner == variant && campaign.phase == 2 {
        return Ok(Json(campaign.view()));
    }
    if !matches!(
        campaign.state,
        CampaignState::Sending | CampaignState::Paused
    ) {
        return Err(campaign_state(campaign.state));
    }
    let Some(variant) =
        variant.filter(|v| usize::from(v.unsigned_abs()) < campaign.variants.0.len())
    else {
        return Err(invalid("variant", "is not one of the campaign's variants"));
    };
    let job = Uuid::now_v7();
    let mut tx = state.pool.begin().await?;
    // `campaigns`, then `jobs` (ADR 0005).
    let n = sqlx::query(WINNER)
        .bind(workspace_id)
        .bind(id)
        .bind(variant)
        .bind(job)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        drop(tx);
        let row = load(&state.pool, workspace_id, id).await?;
        return Err(campaign_state(row.state));
    }
    // Phase 2: the remainder resolves with the winner, starting now.
    sqlx::query(LAUNCH_JOB)
        .bind(job)
        .bind(workspace_id)
        .bind(id)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::CAMPAIGN_WINNER_CHOSEN,
        (workspace_id, id),
        json!({ "variant": variant }),
    )
    .await?;
    let row = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(row.view()))
}

// ---------------------------------------------------------------------------------------------
// Preview, test send, estimate

/// Whether every variant of a frozen rendition still passes `ready` against the fields as they are
/// now, on its OWN key (spec §4.7 ruling 10).
fn frozen_usable(f: &Frozen, defs: &[Definition]) -> bool {
    f.variants
        .iter()
        .all(|v| v.rendition.ready(v.rendition.fingerprint(), defs).is_ok())
}

/// One variant of `frozen` as one recipient gets it: `(subject, html, text)`.
fn render_for(
    frozen: &Frozen,
    defs: &[Definition],
    variant: usize,
    token: &str,
) -> ApiResult<(String, String, String)> {
    let v = frozen
        .variants
        .get(variant)
        .ok_or_else(|| invalid("variant", "is not one of the campaign's variants"))?;
    let attributes = sample(defs);
    let contact = Contact {
        email: "sam.rivera@example.com",
        first_name: "Sam",
        last_name: "Rivera",
        timezone: Some("UTC"),
        attributes: &attributes,
        unsubscribe_token: token,
    };
    // The rendition's OWN key: what is frozen is what is sent, whatever the template says now.
    let rendered = v
        .rendition
        .ready(v.rendition.fingerprint(), defs)
        .and_then(|r| r.render(&contact))
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("{e}")))?;
    Ok((
        render_subject(&v.subject, &contact),
        rendered.html,
        rendered.text,
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/preview",
    operation_id = "preview_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    request_body = CampaignPreviewRequest,
    responses(
        (status = 200, description = "A variant for a sample recipient, system footer included", body = CampaignPreview),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "postal-address-required, sending-domain-unverified or template-stale"),
        (status = 422, description = "an incomplete campaign, or a variant it does not have"),
        (status = 429, description = "rate-limited: the template-preview budget"),
    ),
)]
pub async fn preview_campaign(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<CampaignPreviewRequest>,
) -> ApiResult<Response> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    Ok(limited(&state.template_preview_limit, workspace_id, async {
        let campaign = load(&state.pool, workspace_id, id).await?;
        let defs = definitions_for(&state.pool, workspace_id).await?;
        // A launched campaign shows what it froze: its template may have changed or gone since.
        let kept = match (
            campaign.frozen_at,
            load_frozen(&state.pool, workspace_id, id).await?,
        ) {
            (Some(_), Some(Ok(f))) if frozen_usable(&f, &defs) => Some(f),
            _ => None,
        };
        let frozen = match kept {
            Some(f) => f,
            None => prepare(&state.pool, &state.config.public_url, &campaign)
                .await
                .map_err(refused)?,
        };
        let (subject, html, text) =
            render_for(&frozen, &defs, input.variant.unwrap_or(0), "preview")?;
        Ok(Json(CampaignPreview {
            subject,
            html,
            text,
        }))
    })
    .await)
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/test",
    operation_id = "test_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    request_body = TestSendRequest,
    responses(
        (status = 202, description = "One job queued per address", body = TestSendResult),
        (status = 403, description = "insufficient-role: a test send takes editor"),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 409, description = "campaign-state: only a draft or scheduled campaign is \
            tested; postal-address-required; sending-domain-unverified; or template-stale"),
        (status = 422, description = "no addresses, more than 5, an address that is not one, \
            or an incomplete campaign"),
        (status = 429, description = "rate-limited: five test sends a minute per workspace"),
    ),
)]
pub async fn test_campaign(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<TestSendRequest>,
) -> ApiResult<Response> {
    authorize(&state.pool, &principal, workspace_id, Role::Editor).await?;
    Ok(limited(&state.test_send_limit, workspace_id, async {
        let mut addresses = Vec::new();
        for raw in &input.addresses {
            let address = normalize_email(raw)
                .map_err(|_| invalid("addresses", "holds a value that is not an email address"))?;
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        if addresses.is_empty() || input.addresses.len() > MAX_TEST_ADDRESSES {
            return Err(invalid(
                "addresses",
                &format!("takes 1 to {MAX_TEST_ADDRESSES} addresses"),
            ));
        }
        let campaign = load(&state.pool, workspace_id, id).await?;
        if !matches!(
            campaign.state,
            CampaignState::Draft | CampaignState::Scheduled
        ) {
            return Err(campaign_state(campaign.state));
        }
        let frozen = prepare(&state.pool, &state.config.public_url, &campaign)
            .await
            .map_err(refused)?;
        let defs = definitions_for(&state.pool, workspace_id).await?;
        let variant = input.variant.unwrap_or(0);
        // The sample contact and a token that answers "no longer valid" (spec §4.8): no
        // subscriber's data leaves in a test.
        let (subject, html, text) = render_for(&frozen, &defs, variant, "test")?;
        let from_name = &frozen.variants[variant].from_name;
        let mut tx = state.pool.begin().await?;
        for to in &addresses {
            // `jobs` inserts only (ADR 0005): each takes `for key share` on the campaign.
            sqlx::query(
                "insert into jobs (id, workspace_id, kind, campaign_id, payload) \
                 values ($1, $2, 'campaign_test', $3, $4)",
            )
            .bind(Uuid::now_v7())
            .bind(workspace_id)
            .bind(id)
            .bind(json!({
                "to": to,
                "subject": format!("[Test] {subject}"),
                "html": html,
                "text": text,
                "from": frozen.from,
                "from_name": from_name,
                "reply_to": frozen.reply_to,
                "sending_domain_id": frozen.sending_domain_id,
            }))
            .execute(&mut *tx)
            .await?;
        }
        record(
            &mut tx,
            &meta,
            &principal,
            actions::CAMPAIGN_TEST_SENT,
            (workspace_id, id),
            json!({ "addresses": addresses.len(), "variant": variant }),
        )
        .await?;
        tx.commit().await?;
        Ok((
            StatusCode::ACCEPTED,
            Json(TestSendResult {
                queued: addresses.len(),
            }),
        ))
    })
    .await)
}

/// Postgres' `query_canceled`, which is what `statement_timeout` raises.
fn timed_out(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "57014")
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/campaigns/{id}/estimate",
    operation_id = "estimate_campaign",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Campaign id"),
    ),
    responses(
        (status = 200, description = "`count: null, exact: false` when two seconds were not \
            enough", body = CampaignEstimate),
        (status = 404, description = "not a member, or no such campaign in this workspace"),
        (status = 422, description = "the campaign's segment no longer exists"),
        (status = 429, description = "rate-limited: the segment-preview budget"),
    ),
)]
pub async fn estimate_campaign(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    Ok(limited(&state.preview_limit, workspace_id, async {
        let c = load(&state.pool, workspace_id, id).await?;
        let mut tx = state.pool.begin().await?;
        // Inside a transaction, or `set local` is a warning and the timeout never applies
        // (`segments::preview`'s first measured fact).
        sqlx::query("set local statement_timeout = '2s'")
            .execute(&mut *tx)
            .await?;
        let group = match c.segment_id {
            Some(sid) => Some(
                segments::load_rules(&mut tx, workspace_id, sid)
                    .await
                    .map_err(|e| match e {
                        ApiError::Internal(_) => e,
                        _ => invalid("segment_id", "is not a segment that can be read"),
                    })?,
            ),
            None => None,
        };
        // A contact on two lists is one recipient.
        let mut q = QueryBuilder::<Postgres>::new(
            "select count(distinct l.contact_id) from mailable_list_members l \
             join contacts on contacts.workspace_id = l.workspace_id \
             and contacts.id = l.contact_id where l.workspace_id = ",
        );
        q.push_bind(workspace_id)
            .push(" and l.list_id = any(")
            .push_bind(c.list_ids.clone())
            .push(")");
        if let Some(g) = &group {
            q.push(" and ");
            push_rules(&mut q, g);
        }
        let counted = q.build_query_scalar::<i64>().fetch_one(&mut *tx).await;
        tx.rollback().await?;
        match counted {
            Ok(n) => Ok(Json(CampaignEstimate {
                count: Some(n),
                exact: true,
            })),
            Err(e) if timed_out(&e) => Ok(Json(CampaignEstimate {
                count: None,
                exact: false,
            })),
            Err(e) => Err(e.into()),
        }
    })
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_part_is_a_dot_atom() {
        for ok in ["news", "a.b", "first.last+tag", "x_y-z", "o'brien", "a/b"] {
            assert!(dot_atom(ok), "{ok}");
        }
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "a b",
            "a@b",
            "a,b",
            "a\"b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!dot_atom(bad), "{bad}");
        }
    }

    #[test]
    fn a_local_datetime_is_what_the_input_produces() {
        assert!(parse_local("2027-03-14T02:30").is_some());
        assert!(parse_local("2027-03-14T02:30:00").is_some());
        assert!(parse_local("2027-03-14 02:30").is_none());
        assert!(parse_local("2027-03-14T02:30Z").is_none());
    }
}
