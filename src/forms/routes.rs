// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::{Deserialize, Deserializer};
use sqlx::{PgConnection, types::Json as Jsonb};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    Form, FormField, validate_fields, validate_list_ids, validate_redirect_url, validate_slug,
    validate_text,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::definitions_for,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

const HEADLINE_MAX: usize = 200;
const LABEL_MAX: usize = 100;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/forms",
            get(list_forms).post(create_form),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/forms/{id}",
            get(get_form).patch(update_form).delete(delete_form),
        )
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewForm {
    /// Unique in the workspace. It is what names the form in its public URL.
    pub slug: String,
    /// The operator's name for it.
    pub name: String,
    /// At least one, at most 25. Each must be a list of this workspace.
    pub list_ids: Vec<Uuid>,
    /// The page's inputs. Exactly one must target `email`.
    pub fields: Vec<FormField>,
    /// Absolute `http`/`https` only. Absent or `null` means the form has no redirect.
    pub redirect_url: Option<String>,
    pub headline: String,
    pub button_label: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateForm {
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub slug: Option<Option<String>>,
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub name: Option<Option<String>>,
    /// Replaces the lists outright. Cannot be `null` or empty.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Vec<Uuid>, required = false)]
    pub list_ids: Option<Option<Vec<Uuid>>>,
    /// Replaces the fields outright. Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Vec<FormField>, required = false)]
    pub fields: Option<Option<Vec<FormField>>>,
    /// `null` clears it, leaving the form with no redirect.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub redirect_url: Option<Option<String>>,
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub headline: Option<Option<String>>,
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub button_label: Option<Option<String>>,
}

/// Tells a key that is absent (`None`) from one present as `null` (`Some(None)`). A plain
/// `Option<T>` maps both to `None`, which would silently ignore a `"slug": null`.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
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

/// A fresh `id` can only collide on `(workspace_id, slug)`.
fn slug_taken(e: sqlx::Error) -> ApiError {
    match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("form-slug-taken"),
        _ => e.into(),
    }
}

/// A list id that is not this workspace's. `form_lists` carries composite foreign keys on both
/// `(workspace_id, form_id)` and `(workspace_id, list_id)`; the form row is written first in the
/// same transaction, so the only key that can fail here is the list one.
///
/// The message names no id and is the same whether the id belongs to another workspace or to
/// nobody at all — which is also all the database knows. Cross-workspace reach answers as if the
/// resource did not exist, and must not answer differently from the case where it truly does not.
fn unknown_list(e: sqlx::Error) -> ApiError {
    match e.as_database_error() {
        Some(d) if d.is_foreign_key_violation() => {
            ApiError::Validation(vec![FieldError::new("list_ids", "unknown list")])
        }
        _ => e.into(),
    }
}

/// Appends the audit row for a form write, in the caller's transaction.
///
/// The metadata is the form's **whole definition**, not the list of keys the request named.
/// `consent_log.source_ref` holds a form id, so a consent row says only "this person submitted
/// form X"; without this, an operator editing X's lists or its copy would silently change what
/// every consent row already pointing at X appears to say. For a delete it is the definition as
/// it stood when it was deleted, because the row is about to stop existing.
///
/// A form definition carries no personal data — operator-authored copy, list ids and field
/// targets — so recording all of it is safe under `AuditEvent::metadata`'s rule.
async fn audit_form(
    db: &mut PgConnection,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    form: &Form,
) -> ApiResult<()> {
    audit::record(
        db,
        meta,
        AuditEvent {
            workspace_id: Some(form.workspace_id),
            actor: principal.actor(),
            action,
            target_type: Some("form"),
            target_id: Some(form.id.to_string()),
            metadata: serde_json::to_value(form).map_err(|e| ApiError::Internal(e.into()))?,
        },
    )
    .await?;
    Ok(())
}

/// Replaces the form's lists. One statement per side, so the foreign key sees every id.
async fn set_lists(
    db: &mut PgConnection,
    workspace_id: Uuid,
    form_id: Uuid,
    list_ids: &[Uuid],
) -> ApiResult<()> {
    sqlx::query!(
        "delete from form_lists where workspace_id = $1 and form_id = $2",
        workspace_id,
        form_id,
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!(
        "insert into form_lists (workspace_id, form_id, list_id) \
         select $1, $2, unnest($3::uuid[])",
        workspace_id,
        form_id,
        list_ids,
    )
    .execute(&mut *db)
    .await
    .map_err(unknown_list)?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/forms",
    operation_id = "list_forms",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<Form>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_forms(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Form>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        super::Row,
        r#"select f.id, f.workspace_id, f.slug, f.name,
                  f.fields as "fields: Jsonb<Vec<FormField>>",
                  f.redirect_url, f.headline, f.button_label, f.created_at, f.updated_at,
                  coalesce(array_agg(fl.list_id order by fl.list_id)
                           filter (where fl.list_id is not null), '{}') as "list_ids!: Vec<Uuid>"
           from forms f
           left join form_lists fl on fl.workspace_id = f.workspace_id and fl.form_id = f.id
           where f.workspace_id = $1 and ($2::uuid is null or f.id < $2)
           group by f.id order by f.id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    let forms: Vec<Form> = rows.into_iter().map(Form::from).collect();
    Ok(Json(Page::from_rows(forms, limit, |f| f.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/forms/{id}",
    operation_id = "get_form",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Form id"),
    ),
    responses(
        (status = 200, body = Form),
        (status = 404, description = "not a member, or no such form in this workspace"),
    ),
)]
pub async fn get_form(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Form>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(super::get(&mut conn, workspace_id, id).await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/forms",
    operation_id = "create_form",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewForm,
    responses(
        (status = 201, body = Form),
        (status = 403, description = "insufficient-role: creating a form takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "form-slug-taken"),
        (status = 422, description = "invalid input, an unknown field, or an unknown list"),
    ),
)]
pub async fn create_form(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewForm>,
) -> ApiResult<(StatusCode, Json<Form>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let defs = definitions_for(&mut *tx, workspace_id).await?;

    let slug = validate_slug(&input.slug);
    let name = validate_text("name", &input.name, LABEL_MAX);
    let list_ids = validate_list_ids(&input.list_ids);
    let fields = validate_fields(&input.fields, &defs);
    let redirect_url = input
        .redirect_url
        .as_deref()
        .map(validate_redirect_url)
        .transpose();
    let headline = validate_text("headline", &input.headline, HEADLINE_MAX);
    let button_label = validate_text("button_label", &input.button_label, LABEL_MAX);
    let (Ok(slug), Ok(name), Ok(list_ids), Ok(fields), Ok(redirect_url), Ok(headline), Ok(button)) = (
        &slug,
        &name,
        &list_ids,
        &fields,
        &redirect_url,
        &headline,
        &button_label,
    ) else {
        let errors = [
            slug.err(),
            name.err(),
            list_ids.err(),
            fields.err(),
            redirect_url.err(),
            headline.err(),
            button_label.err(),
        ];
        return Err(ApiError::Validation(errors.into_iter().flatten().collect()));
    };

    let id = Uuid::now_v7();
    sqlx::query!(
        "insert into forms (id, workspace_id, slug, name, fields, redirect_url, headline, \
         button_label) values ($1, $2, $3, $4, $5, $6, $7, $8)",
        id,
        workspace_id,
        slug.as_str(),
        name.as_str(),
        Jsonb(fields) as Jsonb<&Vec<FormField>>,
        redirect_url.as_deref(),
        headline.as_str(),
        button.as_str(),
    )
    .execute(&mut *tx)
    .await
    .map_err(slug_taken)?;
    set_lists(&mut tx, workspace_id, id, list_ids).await?;
    let form = super::get(&mut tx, workspace_id, id).await?;
    audit_form(&mut tx, &meta, &principal, actions::FORM_CREATED, &form).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(form)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/forms/{id}",
    operation_id = "update_form",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Form id"),
    ),
    request_body = UpdateForm,
    responses(
        (status = 200, body = Form),
        (status = 403, description = "insufficient-role: editing a form takes editor"),
        (status = 404, description = "not a member, or no such form in this workspace"),
        (status = 409, description = "form-slug-taken"),
        (status = 422, description = "invalid input, an unknown field, or an unknown list"),
    ),
)]
pub async fn update_form(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateForm>,
) -> ApiResult<Json<Form>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let defs = definitions_for(&mut *tx, workspace_id).await?;

    let mut errors: Vec<FieldError> = Vec::new();
    // `redirect_url` is the one key where `null` means something: sent as null, it clears the
    // column. Every other key refuses null rather than guessing.
    let sets_redirect = input.redirect_url.is_some();
    let slug = not_null("slug", input.slug, &mut errors);
    let name = not_null("name", input.name, &mut errors);
    let list_ids = not_null("list_ids", input.list_ids, &mut errors);
    let fields = not_null("fields", input.fields, &mut errors);
    let headline = not_null("headline", input.headline, &mut errors);
    let button_label = not_null("button_label", input.button_label, &mut errors);

    let mut keep = |r: Result<String, FieldError>| match r {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(e);
            None
        }
    };
    let slug = slug.and_then(|v| keep(validate_slug(&v)));
    let name = name.and_then(|v| keep(validate_text("name", &v, LABEL_MAX)));
    let headline = headline.and_then(|v| keep(validate_text("headline", &v, HEADLINE_MAX)));
    let button_label =
        button_label.and_then(|v| keep(validate_text("button_label", &v, LABEL_MAX)));
    let redirect_url = input
        .redirect_url
        .flatten()
        .and_then(|v| keep(validate_redirect_url(&v)));
    let list_ids = match list_ids.map(|v| validate_list_ids(&v)) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => {
            errors.push(e);
            None
        }
        None => None,
    };
    let fields = match fields.map(|v| validate_fields(&v, &defs)) {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => {
            errors.push(e);
            None
        }
        None => None,
    };
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }

    let updated = sqlx::query!(
        "update forms set slug = coalesce($3::text, slug), name = coalesce($4::text, name), \
         fields = coalesce($5::jsonb, fields), headline = coalesce($6::text, headline), \
         button_label = coalesce($7::text, button_label), \
         redirect_url = case when $8::boolean then $9::text else redirect_url end, \
         updated_at = now() where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        slug.as_deref(),
        name.as_deref(),
        fields.as_ref().map(Jsonb) as Option<Jsonb<&Vec<FormField>>>,
        headline.as_deref(),
        button_label.as_deref(),
        sets_redirect,
        redirect_url.as_deref(),
    )
    .execute(&mut *tx)
    .await
    .map_err(slug_taken)?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    if let Some(list_ids) = &list_ids {
        set_lists(&mut tx, workspace_id, id, list_ids).await?;
    }
    let form = super::get(&mut tx, workspace_id, id).await?;
    audit_form(&mut tx, &meta, &principal, actions::FORM_UPDATED, &form).await?;
    tx.commit().await?;
    Ok(Json(form))
}

/// A hard delete. The form's `form_lists` rows go with it; the lists and their members stay.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/forms/{id}",
    operation_id = "delete_form",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Form id"),
    ),
    responses(
        (status = 204, description = "Deleted, with its list links; the lists and their members stay"),
        (status = 403, description = "insufficient-role: deleting a form takes editor"),
        (status = 404, description = "not a member, or no such form in this workspace"),
    ),
)]
pub async fn delete_form(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    // Read before the delete, not after: the audit row carries the definition the form had when
    // it was deleted, and a moment later there is nothing left to read. This is also the 404 for
    // a form of another workspace, so the statement below cannot reach one.
    let form = super::get(&mut tx, workspace_id, id).await?;
    let deleted = sqlx::query!(
        "delete from forms where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit_form(&mut tx, &meta, &principal, actions::FORM_DELETED, &form).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
