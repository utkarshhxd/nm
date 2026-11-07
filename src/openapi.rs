// SPDX-License-Identifier: AGPL-3.0-only

use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::{Config, SwaggerUi};

const SPEC_PATH: &str = "/api/v1/openapi.json";
const DOCS_PATH: &str = "/api/v1/docs";

#[derive(OpenApi)]
#[openapi(info(title = "NM API"))]
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
