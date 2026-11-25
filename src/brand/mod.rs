// SPDX-License-Identifier: AGPL-3.0-only

//! The brand kit: the logo, colours, fonts and footer a workspace applies to every template.
//! One row per workspace (migration 0032), keys fixed by contract section 8.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgExecutor;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::template::mjml;

pub mod routes;

/// The kit as the API shows it and as the row stores it.
///
/// `palette` and `footer` are passed through **whole**. Contract section 8: a kit written by a
/// newer version must survive a round trip through an older screen rather than being silently
/// stripped, so the known keys are validated and every other key is kept exactly as it arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BrandKit {
    /// `http` or `https` only (contract section 11): `mailto:` and `tel:` are meaningless as an
    /// image source, so the logo takes a narrower set than the link allowlist.
    pub logo_url: Option<String>,
    /// `{ "background", "text", "accent", "accent_text" }`, each `#rgb` or `#rrggbb`. Other keys
    /// are kept untouched, which is why the schema is an open map and not a closed object: a
    /// client has to be able to send back a key its own version does not know.
    #[schema(value_type = std::collections::HashMap<String, Value>)]
    pub palette: Value,
    pub font_stack: Option<String>,
    /// `{ "text", "address" }`. `address` is the CAN-SPAM / CASL postal address. An open map, for
    /// the same reason as `palette`.
    #[schema(value_type = std::collections::HashMap<String, Value>)]
    pub footer: Value,
}

impl BrandKit {
    /// A workspace with no row has an empty kit, never a 404: the screen must be reachable on day
    /// one, when no workspace has saved one.
    pub fn empty() -> BrandKit {
        BrandKit {
            logo_url: None,
            palette: json!({}),
            font_stack: None,
            footer: json!({}),
        }
    }

    /// The renderer's view of this kit. Unknown keys are dropped here and only here: they are
    /// stored, they are returned, and they do not reach the emitter.
    pub fn render(&self) -> mjml::BrandKit {
        mjml::BrandKit::from_row(
            self.logo_url.as_deref(),
            &self.palette,
            self.font_stack.as_deref(),
            &self.footer,
        )
    }
}

/// The workspace's kit, or an empty one when it has no row.
pub async fn load(db: impl PgExecutor<'_>, workspace_id: Uuid) -> sqlx::Result<BrandKit> {
    let row = sqlx::query!(
        "select logo_url, palette, font_stack, footer from brand_kits where workspace_id = $1",
        workspace_id,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map_or_else(BrandKit::empty, |r| BrandKit {
        logo_url: r.logo_url,
        palette: r.palette,
        font_stack: r.font_stack,
        footer: r.footer,
    }))
}
