// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State};
use serde_json::{Value, json};
use uuid::Uuid;

use super::BrandKit;
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    template::mjml::BrandError,
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/v1/workspaces/{workspace_id}/brand-kit",
        axum::routing::get(get_brand_kit).put(save_brand_kit),
    )
}

/// The keys this product reads out of `palette` and `footer`. Anything else is an operator's or a
/// newer version's, and is stored and returned untouched.
const PALETTE_KEYS: [&str; 4] = ["background", "text", "accent", "accent_text"];
const FOOTER_KEYS: [&str; 2] = ["text", "address"];

/// A known key whose value is present must be a string, before the colour grammar sees it.
///
/// `mjml::BrandKit::from_row` reads these with `as_str` and drops anything else, so a
/// `"accent": 7` would otherwise be stored, returned, and silently ignored by every render — a
/// kit that says one thing to the operator and another to the emitter.
fn strings(object: &Value, field: &str, keys: &[&str], errors: &mut Vec<FieldError>) {
    for key in keys {
        match object.get(key) {
            None | Some(Value::Null) | Some(Value::String(_)) => {}
            Some(_) => errors.push(FieldError::new(
                format!("{field}.{key}"),
                "must be a string",
            )),
        }
    }
}

/// `BrandKit::validate` is the ONE brand-kit validator (the font-stack grammar and the colour
/// grammar live with the emitter that uses them). This maps its refusal onto the field the
/// operator typed into.
fn field_error(e: BrandError) -> FieldError {
    let field = match e {
        BrandError::Colour { field } => format!("palette.{field}"),
        BrandError::FontStack => "font_stack".to_owned(),
        BrandError::Logo => "logo_url".to_owned(),
    };
    FieldError::new(field, e.to_string())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/brand-kit",
    operation_id = "get_brand_kit",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "A workspace with no kit gets an empty one, never a 404.",
            body = BrandKit),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn get_brand_kit(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
) -> ApiResult<Json<BrandKit>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(super::load(&mut *conn, workspace_id).await?))
}

/// The whole kit, replaced. **Admin and above** — contract section 9: `footer.address` is the
/// postal address CAN-SPAM and CASL require, so this is compliance configuration and not content
/// styling. An editor composes emails; an admin configures what every email the workspace sends is
/// legally obliged to say.
#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{workspace_id}/brand-kit",
    operation_id = "save_brand_kit",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BrandKit,
    responses(
        (status = 200, description = "The kit as stored. Unknown palette and footer keys survive.",
            body = BrandKit),
        (status = 403, description = "insufficient-role: the brand kit takes admin"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a colour outside `#rgb`/`#rrggbb`, a font stack that is not \
            a list of family names, a logo that is not http or https, a palette or footer that is \
            not an object, or an unknown field"),
    ),
)]
pub async fn save_brand_kit(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BrandKit>,
) -> ApiResult<Json<BrandKit>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let mut errors = Vec::new();
    // The column's check constraint refuses a non-object too; this is the one that names the
    // field, because `null`, `7` and `[]` all arrive as valid JSON for a `Value`.
    for (field, v) in [("palette", &input.palette), ("footer", &input.footer)] {
        if !v.is_object() {
            errors.push(FieldError::new(field, "must be a JSON object"));
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    strings(&input.palette, "palette", &PALETTE_KEYS, &mut errors);
    strings(&input.footer, "footer", &FOOTER_KEYS, &mut errors);
    if let Err(e) = input.render().validate() {
        errors.push(field_error(e));
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    // ONE locking statement (ADR 0005, the M2b section): the insert arm takes `for key share` on
    // the workspace row through `brand_kits_workspace_id_fkey`, the update arm names no key column
    // and so takes nothing. Nothing else here locks a row.
    sqlx::query!(
        "insert into brand_kits (workspace_id, logo_url, palette, font_stack, footer) \
         values ($1, $2, $3, $4, $5) \
         on conflict (workspace_id) do update set logo_url = excluded.logo_url, \
         palette = excluded.palette, font_stack = excluded.font_stack, \
         footer = excluded.footer, updated_at = now()",
        workspace_id,
        input.logo_url,
        input.palette,
        input.font_stack,
        input.footer,
    )
    .execute(&mut *tx)
    .await?;
    // Which keys were set, never their values: the footer carries a postal address and the audit
    // log is kept.
    let keys = |v: &Value| -> Vec<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::BRAND_KIT_UPDATED,
            target_type: Some("brand_kit"),
            target_id: Some(workspace_id.to_string()),
            metadata: json!({
                "palette_keys": keys(&input.palette),
                "footer_keys": keys(&input.footer),
                "has_logo": input.logo_url.is_some(),
                "has_font_stack": input.font_stack.is_some(),
            }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(input))
}
