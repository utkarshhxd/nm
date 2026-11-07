// SPDX-License-Identifier: AGPL-3.0-only

use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::{Config, SwaggerUi};

const SPEC_PATH: &str = "/api/v1/openapi.json";
const DOCS_PATH: &str = "/api/v1/docs";

#[derive(OpenApi)]
#[openapi(
    info(title = "NM API"),
    paths(
        crate::auth::login::login,
        crate::auth::login::logout,
        crate::auth::login::me,
        crate::auth::login::change_password,
        crate::auth::totp::setup,
        crate::auth::totp::confirm,
        crate::auth::totp::disable,
        crate::auth::totp::login_totp,
        crate::workspaces::list,
        crate::workspaces::create,
        crate::workspaces::show,
        crate::workspaces::update,
        crate::workspaces::members::list,
        crate::workspaces::members::change_role,
        crate::workspaces::members::remove,
        crate::workspaces::invites::create,
        crate::workspaces::invites::list,
        crate::workspaces::invites::revoke,
        crate::workspaces::invites::preview,
        crate::workspaces::invites::accept,
        crate::audit::routes::list,
        crate::auth::api_keys::create,
        crate::auth::api_keys::list,
        crate::auth::api_keys::revoke,
    ),
    components(schemas(
        crate::auth::login::LoginRequest,
        crate::auth::login::LoginResponse,
        crate::auth::login::MeResponse,
        crate::auth::login::ChangePasswordRequest,
        crate::auth::totp::SetupResponse,
        crate::auth::totp::ConfirmRequest,
        crate::auth::totp::ConfirmResponse,
        crate::auth::totp::DisableRequest,
        crate::auth::totp::SecondFactorRequest,
        crate::workspaces::roles::Role,
        crate::workspaces::Workspace,
        crate::workspaces::MeWorkspace,
        crate::workspaces::NewWorkspace,
        crate::workspaces::UpdateWorkspace,
        crate::workspaces::members::Member,
        crate::workspaces::members::ChangeRole,
        crate::workspaces::invites::NewInvite,
        crate::workspaces::invites::CreatedInvite,
        crate::workspaces::invites::InviteSummary,
        crate::workspaces::invites::InvitePreview,
        crate::workspaces::invites::AcceptInvite,
        crate::workspaces::invites::Accepted,
        crate::audit::routes::AuditEntry,
        crate::auth::api_keys::NewApiKey,
        crate::auth::api_keys::CreatedApiKey,
        crate::auth::api_keys::ApiKeySummary,
    ))
)]
struct ApiDoc;

/// The OpenAPI document; `info.version` is this crate's version. Route handlers
/// register themselves here as later tasks add them.
pub fn spec() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// `GET /api/v1/openapi.json` and the Swagger UI at `/api/v1/docs`. The UI
/// assets are compiled into the binary (`vendored`), and the spec validator badge
/// is disabled: its default points at validator.swagger.io, a third-party call.
pub fn docs<S: Clone + Send + Sync + 'static>() -> Router<S> {
    SwaggerUi::new(DOCS_PATH)
        .url(SPEC_PATH, spec())
        .config(Config::new([SPEC_PATH]).validator_url("none"))
        .into()
}
