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
        crate::auth::passkeys::register_start,
        crate::auth::passkeys::register_finish,
        crate::auth::passkeys::list,
        crate::auth::passkeys::remove,
        crate::auth::passkeys::login_start,
        crate::auth::passkeys::login_finish,
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
        crate::auth::passkeys::ChallengeResponse,
        crate::auth::passkeys::NewPasskey,
        crate::auth::passkeys::FinishRequest,
        crate::auth::passkeys::CreatedPasskey,
        crate::auth::passkeys::PasskeySummary,
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

#[cfg(test)]
mod tests {
    use super::spec;
    use serde_json::Value;
    use std::collections::BTreeMap;

    /// `operationId` must be unique across the document (OpenAPI 3.1 §4.8.10.3).
    ///
    /// utoipa derives it from the handler's **function name**, so two modules that each export a
    /// `list` silently ship the same id, and nothing in the Rust build notices. The damage lands
    /// in the generated TypeScript client: `openapi-typescript` keys one `operations` interface by
    /// that id, so every colliding path takes on whichever duplicate won — a members GET typed as
    /// the passkey list, with no path parameters. `skipLibCheck` hides the duplicate identifiers
    /// it emits, so the symptom is a pile of type errors naming schemas nobody touched.
    ///
    /// This walks the serialised document rather than `PathItem`'s eight method fields, so it
    /// checks exactly what is published and does not need updating if utoipa adds a verb.
    #[test]
    fn operation_ids_are_unique() {
        let doc = serde_json::to_value(spec()).expect("the spec serialises");
        let mut seen: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (path, item) in doc["paths"].as_object().expect("paths is an object") {
            // A path item also holds `summary`, `parameters` and `$ref`; only an operation has an
            // `operationId`, so this needs no list of verbs to filter by.
            for (method, operation) in item.as_object().expect("a path item is an object") {
                if let Some(id) = operation.get("operationId").and_then(Value::as_str) {
                    seen.entry(id).or_default().push(format!("{method} {path}"));
                }
            }
        }
        assert!(
            !seen.is_empty(),
            "no operationId found at all: the walk is wrong, not the document"
        );
        let duplicates: Vec<_> = seen.iter().filter(|(_, used)| used.len() > 1).collect();
        assert!(
            duplicates.is_empty(),
            "duplicate operationId {duplicates:?} — utoipa derives it from the handler's function \
             name, so give each one an explicit `operation_id = \"…\"` in its `#[utoipa::path(…)]`"
        );
    }
}
