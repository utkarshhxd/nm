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
        crate::consent::routes::list_consent,
        crate::auth::api_keys::create,
        crate::auth::api_keys::list,
        crate::auth::api_keys::revoke,
        crate::custom_fields::routes::list_custom_fields,
        crate::custom_fields::routes::create_custom_field,
        crate::custom_fields::routes::update_custom_field,
        crate::custom_fields::routes::archive_custom_field,
        crate::custom_fields::routes::purge_custom_field,
        crate::contacts::routes::list_contacts,
        crate::contacts::routes::get_contact,
        crate::contacts::routes::contact_eligibility,
        crate::tags::routes::list_contact_tags,
        crate::contacts::routes::create_contact,
        crate::contacts::routes::update_contact,
        crate::contacts::routes::delete_contact,
        crate::contacts::routes::erase_contact,
        crate::contacts::routes::create_selection,
        crate::contacts::routes::bulk_tag_contacts,
        crate::contacts::routes::bulk_untag_contacts,
        crate::contacts::routes::bulk_subscribe_contacts,
        crate::contacts::routes::bulk_unsubscribe_contacts,
        crate::contacts::routes::bulk_delete_contacts,
        crate::contacts::routes::export_contacts,
        crate::contacts::import::routes::create_import,
        crate::contacts::import::routes::start_import,
        crate::contacts::import::routes::list_imports,
        crate::contacts::import::routes::get_import,
        crate::contacts::import::routes::list_import_errors,
        crate::lists::routes::list_lists,
        crate::lists::routes::get_list,
        crate::lists::routes::create_list,
        crate::lists::routes::update_list,
        crate::lists::routes::delete_list,
        crate::lists::routes::mailable_count,
        crate::lists::routes::list_list_members,
        crate::lists::routes::add_list_member,
        crate::lists::routes::unsubscribe_list_member,
        crate::lists::routes::list_contact_lists,
        crate::lists::routes::add_contact_to_list,
        crate::lists::routes::unsubscribe_contact_from_list,
        crate::forms::routes::list_forms,
        crate::forms::routes::get_form,
        crate::forms::routes::create_form,
        crate::forms::routes::update_form,
        crate::forms::routes::delete_form,
        crate::segments::routes::list_segments,
        crate::segments::routes::get_segment,
        crate::segments::routes::create_segment,
        crate::segments::routes::update_segment,
        crate::segments::routes::delete_segment,
        crate::segments::routes::preview_segment,
        crate::segments::routes::count_segments,
        crate::tags::routes::list_tags,
        crate::tags::routes::create_tag,
        crate::tags::routes::update_tag,
        crate::tags::routes::delete_tag,
        crate::tags::routes::apply_tag,
        crate::tags::routes::remove_tag,
        crate::suppressions::routes::list_suppressions,
        crate::suppressions::routes::create_suppression,
        crate::suppressions::routes::lift_suppression,
        crate::suppressions::routes::list_instance_suppressions,
        crate::suppressions::routes::create_instance_suppression,
        crate::suppressions::routes::lift_instance_suppression,
        crate::relay::routes::list_relays,
        crate::relay::routes::get_relay,
        crate::relay::routes::create_relay,
        crate::relay::routes::update_relay,
        crate::relay::routes::delete_relay,
        crate::sending_domains::routes::list_sending_domains,
        crate::sending_domains::routes::get_sending_domain,
        crate::sending_domains::routes::create_sending_domain,
        crate::sending_domains::routes::update_sending_domain,
        crate::sending_domains::routes::delete_sending_domain,
        crate::sending_domains::routes::check_sending_domain,
        crate::template::routes::list_templates,
        crate::template::routes::get_template,
        crate::template::routes::create_template,
        crate::template::routes::update_template,
        crate::template::routes::delete_template,
        crate::template::routes::preview_template,
        crate::brand::routes::get_brand_kit,
        crate::brand::routes::save_brand_kit,
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
        crate::consent::Action,
        crate::consent::Source,
        crate::consent::routes::ConsentEntry,
        crate::auth::api_keys::NewApiKey,
        crate::auth::api_keys::CreatedApiKey,
        crate::auth::api_keys::ApiKeySummary,
        crate::custom_fields::FieldKind,
        crate::custom_fields::Definition,
        crate::custom_fields::routes::NewCustomField,
        crate::custom_fields::routes::UpdateCustomField,
        crate::custom_fields::routes::Archived,
        crate::custom_fields::routes::Purged,
        crate::contacts::Contact,
        crate::contacts::routes::Eligibility,
        crate::suppressions::SuppressionBlock,
        crate::suppressions::BlockLevel,
        crate::pagination::sorted::ContactSort,
        crate::pagination::sorted::Dir,
        crate::contacts::NewContact,
        crate::contacts::routes::UpdateContact,
        crate::contacts::routes::NewSelection,
        crate::contacts::routes::SelectionToken,
        crate::contacts::routes::BulkTarget,
        crate::contacts::routes::BulkTag,
        crate::contacts::routes::BulkList,
        crate::contacts::routes::BulkResult,
        crate::contacts::import::ContactField,
        crate::contacts::import::ColumnTarget,
        crate::contacts::import::Dedup,
        crate::contacts::import::routes::NewImport,
        crate::contacts::import::routes::StartImport,
        crate::contacts::import::routes::Upload,
        crate::contacts::import::routes::UploadColumn,
        crate::contacts::import::routes::ImportReport,
        crate::contacts::import::routes::ImportErrorRow,
        crate::lists::List,
        crate::lists::ContactList,
        crate::lists::ListMember,
        crate::lists::MembershipStatus,
        crate::lists::Visibility,
        crate::lists::routes::NewList,
        crate::lists::routes::UpdateList,
        crate::lists::routes::NewMember,
        crate::lists::routes::NewContactList,
        crate::lists::routes::Membership,
        crate::lists::routes::MailableCount,
        crate::forms::Form,
        crate::forms::FormField,
        crate::forms::FormTarget,
        crate::forms::routes::NewForm,
        crate::forms::routes::UpdateForm,
        crate::segments::Segment,
        crate::segments::routes::NewSegment,
        crate::segments::routes::UpdateSegment,
        crate::segments::routes::PreviewRequest,
        crate::segments::preview::Preview,
        crate::segments::preview::SegmentCount,
        crate::segments::preview::CountState,
        crate::tags::Tag,
        crate::tags::routes::NewTag,
        crate::tags::routes::UpdateTag,
        crate::suppressions::Suppression,
        crate::suppressions::NewSuppression,
        crate::suppressions::Reason,
        crate::suppressions::Scope,
        crate::relay::Security,
        crate::relay::routes::RelayView,
        crate::relay::routes::NewRelay,
        crate::relay::routes::UpdateRelay,
        crate::sending_domains::Signing,
        crate::sending_domains::CheckResult,
        crate::sending_domains::DnsChecks,
        crate::sending_domains::routes::SendingDomainView,
        crate::sending_domains::routes::NewSendingDomain,
        crate::sending_domains::routes::UpdateSendingDomain,
        crate::template::Kind,
        crate::template::Doc,
        crate::template::block::Block,
        crate::template::block::Run,
        crate::template::block::Mark,
        crate::template::lint::Check,
        crate::template::lint::Severity,
        crate::template::lint::Rule,
        crate::template::lint::Finding,
        crate::template::routes::TemplateView,
        crate::template::routes::NewTemplate,
        crate::template::routes::UpdateTemplate,
        crate::template::routes::TemplatePreviewRequest,
        crate::template::routes::PreviewResult,
        crate::brand::BrandKit,
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

    /// A schema name must be unique too, and **the document cannot be walked to find out.**
    ///
    /// utoipa derives a schema's name from the Rust type's name, and `components.schemas` is one
    /// flat map keyed by it. Two registered types that share a name — `consent::Action` and a
    /// future `audit::Action`, say — do not collide loudly: the second insert replaces the first,
    /// the map still holds one well-formed entry under that key, and every `$ref` to it still
    /// resolves. One of the two types then ships to every client with the other's shape, and the
    /// generated TypeScript compiles, because `Action` exists and is a valid type. Nothing in the
    /// Rust build, the document, or `openapi-typescript` says a word.
    ///
    /// So this reads the registration list rather than the result. The name utoipa uses is the
    /// type path's last segment, which is checked rather than assumed: every name derived here
    /// must appear as a key in the generated document. That second assertion is what keeps the
    /// first honest — if the derivation rule ever stops holding (a `#[schema(as = "…")]` rename,
    /// or a utoipa change), the names compared above would be the wrong strings and this fails
    /// instead of passing silently.
    ///
    /// The reverse containment is deliberately not asserted: `Page_Form` and its siblings are
    /// generated from `body = Page<Form>` on an operation and are in no list.
    #[test]
    fn schema_names_are_unique() {
        let source = include_str!("openapi.rs");
        let list = source
            .split_once("components(schemas(")
            .expect("the schema registration list is spelt `components(schemas(`")
            .1;
        let list = &list[..list.find("))").expect("the list is closed with `))`")];
        let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        // **Split on the comma, not on the newline.** One registration per line is a hand
        // convention and nothing enforces it: `components(schemas(…))` is an attribute token tree
        // and rustfmt does not reformat it. Parsing by line, two paths written on one line —
        // `crate::consent::Action, crate::audit::Action,` — collapsed into a single entry whose
        // `rsplit("::")` yields `Action`, so the pair recorded as one name, no duplicate was seen,
        // and the collision this test exists to catch shipped. The cross-check below did not
        // catch it either: the collapsed key `Action` *is* a published schema name — the
        // surviving one of the two — so the one case it was written for is the one case it
        // passed. Comments are cut at `//` first, whole-line and trailing alike, because a type
        // path never contains one.
        for path in list
            .lines()
            .map(|line| line.split("//").next().unwrap_or(line))
            .flat_map(|line| line.split(','))
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            seen.entry(path.rsplit("::").next().expect("rsplit yields one part"))
                .or_default()
                .push(path);
        }
        assert!(
            seen.len() > 50,
            "only {} schemas parsed out of the registration list: the parse is wrong, not the list",
            seen.len()
        );

        let doc = serde_json::to_value(spec()).expect("the spec serialises");
        let schemas = doc["components"]["schemas"]
            .as_object()
            .expect("components.schemas is an object");
        let unpublished: Vec<_> = seen
            .keys()
            .filter(|name| !schemas.contains_key(**name))
            .collect();
        assert!(
            unpublished.is_empty(),
            "{unpublished:?} are registered but are not keys of components.schemas, so a schema's \
             published name is no longer its type path's last segment — the uniqueness check below \
             would be comparing the wrong strings. Teach this test the new rule before trusting it."
        );

        let duplicates: Vec<_> = seen.iter().filter(|(_, paths)| paths.len() > 1).collect();
        assert!(
            duplicates.is_empty(),
            "two types share a schema name {duplicates:?} — one of them silently replaced the \
             other in components.schemas and is shipping with the wrong shape. Rename one with \
             `#[schema(as = …)]`."
        );
    }
}
