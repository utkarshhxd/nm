// SPDX-License-Identifier: AGPL-3.0-only

//! Templates: the merge-tag language, compiled at save and rendered at send.
//!
//! ```text
//! save, once:        authored document --compile--> Stored { marked HTML, marked text, AST }
//! send, per contact: Stored --ready(fingerprint, definitions)--> Ready --render--> one message
//! ```
//!
//! `mrml` runs **once, at save**. The document that was classified is the document that is sent:
//! at send the stored marked renditions are substituted into and nothing is re-rendered. Running
//! `mrml` again per contact would classify one document and encode against another, which is the
//! CSS injection this whole module is shaped to prevent.
//!
//! **A write to `brand_kits` invalidates every stored `blocks` rendition in that workspace at
//! once** (a raw `mjml` or `html` template never reads the kit, so it is not in the key). Issue
//! 49's answer is to recompile ON READ, in memory, and never write the result back: a campaign
//! [`freeze`]s a fresh compile at launch, and a stored-template preview recompiles a `Stale` one.
//! Neither runs at send. What still refuses is [`StoredError::FieldsChanged`]: a kit or renderer
//! change does not change what a tag means, a field-kind change does, and that needs a human.
//!
//! ```text
//! launch, once:      authored --compile_for_send (+ system footer)--> Stored, frozen on the campaign
//! ```

pub mod block;
mod footer;
pub mod lint;
pub mod mjml;
pub mod render;
pub mod routes;
pub mod tag;
pub mod text;

pub use footer::SystemFooter;

/// The system footer's one slot: the merge-tag name of the recipient's unsubscribe token. Only
/// `tag::Registry::system_footer()` knows it; an operator registry never does, whatever the
/// workspace's custom fields are called.
pub const UNSUBSCRIBE_TOKEN: &str = "unsubscribe_token";

/// The system footer's second slot: the merge-tag name of the message's tracking token, in the
/// open pixel's URL (M2d spec §4.2). As product-only as [`UNSUBSCRIBE_TOKEN`].
pub const TRACKING_TOKEN: &str = "tracking_token";

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use crate::custom_fields::{Definition, FieldKind};
use block::Block;
use mjml::{BrandKit, CompileError};
use render::{Contact, MARKER_FORMAT_VERSION, RenderError};
use tag::Node;
use text::TextTemplate;

/// The versions of the two crates whose OUTPUT is stored, not merely consulted.
///
/// `to_mjml` and `render_mjml` are deterministic, but only per dependency version: after an
/// `mrml` or `url` bump a re-save classifies different bytes, and `MARKER_FORMAT_VERSION` does not
/// cover that — it versions the marker scheme, not the document those markers sit in. Both go into
/// [`fingerprint`], so a bump invalidates every stored rendition. A stale stored classification is
/// a security bug, not a cache miss.
///
/// Hand-written and held to `Cargo.toml` by `dependency_versions_match_cargo_toml`, rather than
/// read from a build script: the test fails loudly on the bump that would otherwise leave every
/// stored document claiming to have been produced by a version that no longer exists.
pub const MRML_VERSION: &str = "6.0.1";
/// See [`MRML_VERSION`].
pub const URL_VERSION: &str = "2.5.8";

/// How a template is authored. The database stores it as text under a `check` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Kind {
    /// The visual editor's block tree, in `templates.tree`.
    Blocks,
    /// MJML the operator wrote, in `templates.source`.
    Mjml,
    /// HTML the operator wrote, in `templates.source`. `mrml` never runs over it.
    Html,
}

/// The block tree as it crosses the wire and as `templates.tree` stores it: an object, because the
/// column's `templates_tree_is_object` check refuses a bare array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Doc {
    pub blocks: Vec<Block>,
}

/// What the operator wrote, in the one form the compiler takes it.
#[derive(Debug, Clone, Copy)]
pub enum Authored<'a> {
    Blocks(&'a [Block]),
    Mjml(&'a str),
    Html(&'a str),
}

impl Authored<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            Authored::Blocks(_) => Kind::Blocks,
            Authored::Mjml(_) => Kind::Mjml,
            Authored::Html(_) => Kind::Html,
        }
    }
}

/// A field a stored AST names, and what the workspace's schema said about it at the moment that
/// AST was compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Drift {
    pub key: String,
    /// The kind the field had when the template was compiled.
    pub was: FieldKind,
    /// Its kind now, or `null` when the field has been purged.
    pub now: Option<FieldKind>,
}

/// Why a stored rendition may not be used.
///
/// Every arm means *do not substitute into this document*. None of them is recoverable by
/// guessing; each is recoverable by compiling the authored document again.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoredError {
    #[error(
        "this template was compiled by a different version of the renderer (marker format \
         {stored}; this build writes {MARKER_FORMAT_VERSION}). Open it and save it again."
    )]
    MarkerFormat { stored: u32 },
    #[error("the stored template could not be read: {0}. Open it and save it again.")]
    Malformed(String),
    #[error(
        "the stored template nests merge-tag blocks deeper than {} levels, which no renderer will \
         walk. Open it and save it again.",
        tag::MAX_DEPTH
    )]
    TooDeep,
    #[error(
        "the stored rendition was built from a different block tree, brand kit or renderer \
         version. Open it and save it again."
    )]
    Stale,
    #[error("{}", drift_message(.0))]
    FieldsChanged(Vec<Drift>),
    #[error(
        "the plain-text part would have been built from the unsubstituted template, so it would \
         carry the renderer's own markers to the recipient"
    )]
    MarkedText,
    #[error("{}", mjml::render_message(.0))]
    Render(RenderError),
    #[error("this message has no unsubscribe token, so its unsubscribe link would be dead")]
    NoUnsubscribeToken,
    #[error(
        "this message has an empty tracking token, so its pixel and tracked links would be dead"
    )]
    NoTrackingToken,
    #[error("the {0} part of this message lost its unsubscribe link, so it was not built")]
    FooterLost(&'static str),
}

fn drift_message(drift: &[Drift]) -> String {
    let names: Vec<&str> = drift.iter().map(|d| d.key.as_str()).collect();
    format!(
        "this template was compiled against custom fields that have since changed type or been \
         purged ({}), so its merge tags no longer mean what they meant when it was saved. Open it \
         and save it again.",
        names.join(", ")
    )
}

/// Why a campaign's rendition could not be frozen at launch.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FreezeError {
    /// The template's tags were saved against fields that have since changed type or been
    /// purged. A recompile would silently change what they mean, so a human re-saves first.
    #[error("{}", drift_message(.0))]
    Drift(Vec<Drift>),
    #[error("the template cannot be compiled for sending: {0}")]
    Compile(CompileError),
}

/// Both renditions of one template, with everything needed to decide later whether they may still
/// be used. Stored whole in `templates.compiled`.
///
/// There is deliberately **no `Deserialize`**: the only way one of these comes back out of the
/// database is [`Stored::from_column`], which re-checks the marker format and the AST's depth.
/// `serde` re-validates nothing, and a tree that arrived through a column is not self-proving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stored {
    marker_format_version: u32,
    /// [`fingerprint`] of the authored document, the brand kit and the dependency versions that
    /// produced the two renditions below.
    fingerprint: String,
    /// The AST the HTML rendition is filled from. The plain-text part has its OWN AST, in
    /// `text.nodes` — what makes the two parts agree is not that there is one tree, it is that
    /// both are compiled in one call from one `Registry` at one instant and written in one
    /// statement. `compile` is the only thing that can break that, and it is one function.
    nodes: Vec<Node>,
    /// `mrml`'s output with the markers still in it, already classified. The document that is
    /// sent.
    html: String,
    /// The marked plain-text part. `None` for a raw template, which has no block tree to build one
    /// from: its text part is derived from the SUBSTITUTED HTML, per contact.
    text: Option<TextTemplate>,
    /// The custom fields the AST names, and the kind each had at compile time.
    fields: BTreeMap<String, FieldKind>,
    /// A send rendition's link prefix, `{public_url}/u/`; `None` for a template's own rendition.
    /// [`Ready::render`] refuses a message whose parts do not both carry it with the token.
    unsubscribe: Option<String>,
    /// A send rendition's link table (M2d spec §4.3.3): the canonical URLs its tracked hrefs point
    /// at, in first-appearance order; a link's index is its position. Empty, and not written, for
    /// a template's own rendition, and read as empty from one frozen before tracking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    links: Vec<String>,
}

/// `Stored`'s wire shape, and the only `Deserialize` in this file. Private, so nothing outside
/// [`Stored::from_column`] can turn a column into a usable value.
#[derive(Deserialize)]
struct Raw {
    marker_format_version: u32,
    fingerprint: String,
    nodes: Vec<Node>,
    html: String,
    text: Option<TextTemplate>,
    fields: BTreeMap<String, FieldKind>,
    #[serde(default)]
    unsubscribe: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    links: Vec<String>,
}

/// A [`Stored`] that has passed every check a send depends on. Holding one is the proof the checks
/// ran; [`Ready::render`] is the only way to substitute, and it is reachable only from here.
#[derive(Debug)]
pub struct Ready<'a>(&'a Stored);

/// One contact's copy of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub html: String,
    pub text: String,
}

impl Stored {
    /// Read `templates.compiled`. **The only way in.**
    pub fn from_column(v: &Value) -> Result<Stored, StoredError> {
        let raw: Raw =
            serde_json::from_value(v.clone()).map_err(|e| StoredError::Malformed(e.to_string()))?;
        // The version lives beside the document, never inside the markers: putting it in the
        // markers would make it part of the thing being versioned. An unrecognised one is refused
        // rather than guessed at — a boundary marker read as a field marker suppresses the wrong
        // range of a sent message, silently.
        if raw.marker_format_version != MARKER_FORMAT_VERSION {
            return Err(StoredError::MarkerFormat {
                stored: raw.marker_format_version,
            });
        }
        tag::check_depth(&raw.nodes).map_err(|_| StoredError::TooDeep)?;
        if let Some(t) = &raw.text {
            tag::check_depth(&t.nodes).map_err(|_| StoredError::TooDeep)?;
        }
        Ok(Stored {
            marker_format_version: raw.marker_format_version,
            fingerprint: raw.fingerprint,
            nodes: raw.nodes,
            html: raw.html,
            text: raw.text,
            fields: raw.fields,
            unsubscribe: raw.unsubscribe,
            links: raw.links,
        })
    }

    /// Everything that must hold before a contact's copy may be made. Once per send, not once per
    /// contact: [`Ready`] is cheap to use and impossible to obtain without this.
    ///
    /// `fingerprint` is [`fingerprint`] of the authored document and the brand kit as they are
    /// NOW. A mismatch means the stored classification describes a document that is no longer the
    /// one this template would produce — a stale classification, which is a security bug and not a
    /// cache miss, so it is refused rather than used.
    ///
    /// Drift is checked BEFORE the fingerprint, and the order is load-bearing: `Stale` is
    /// recoverable by recompiling in memory (issue 49) and `FieldsChanged` is not, so a rendition
    /// that is both must answer `FieldsChanged`, or the recompile quietly re-reads every tag
    /// against the new field kinds.
    pub fn ready<'a>(
        &'a self,
        fingerprint: &str,
        defs: &[Definition],
    ) -> Result<Ready<'a>, StoredError> {
        let drift: Vec<Drift> = self
            .fields
            .iter()
            .filter_map(|(key, was)| {
                let now = defs.iter().find(|d| &d.key == key).map(|d| d.kind);
                // An ARCHIVED field is not drift, deliberately: a template compiled before the
                // archive still renders (R2's rule), and the contact's values are still there
                // until a purge. A kind change or a purge IS drift: the AST holds an assumption
                // the schema no longer backs.
                (now != Some(*was)).then(|| Drift {
                    key: key.clone(),
                    was: *was,
                    now,
                })
            })
            .collect();
        if !drift.is_empty() {
            return Err(StoredError::FieldsChanged(drift));
        }
        if self.fingerprint != fingerprint {
            return Err(StoredError::Stale);
        }
        Ok(Ready(self))
    }
}

impl Stored {
    /// The custom fields this document's merge tags name.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }

    /// The key this rendition was compiled under. A frozen campaign rendition is used with its
    /// OWN key — a kit edited after launch must not change an in-flight campaign.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// A send rendition's link prefix, `{public_url}/u/`, as it was frozen; `None` for a
    /// template's own rendition. The send builds `List-Unsubscribe` from it, so the header carries
    /// the very URL [`Ready::render`] checked the body for.
    pub fn unsubscribe_base(&self) -> Option<&str> {
        self.unsubscribe.as_deref()
    }

    /// A send rendition's link table, frozen with it; empty for anything untracked.
    pub fn links(&self) -> &[String] {
        &self.links
    }
}

/// The custom fields the STORED version of a document names, read straight out of the column.
///
/// Deliberately WITHOUT the checks [`Stored::from_column`] makes, and safe because of the one
/// thing a caller may do with it: let a re-save keep naming a field that is **archived in this
/// workspace's own `custom_fields`**. The widening is the intersection of this set with that
/// table (see `tag::Registry::allowing`), so a forged or unreadable column cannot invent a field,
/// cannot un-archive one, and cannot reach another workspace's. The worst it can do is let a save
/// keep a reference the operator was entitled to keep anyway.
///
/// Unvalidated on purpose, because the alternative is worse: a row this build cannot read would
/// otherwise take the allowance away and put the operator back in the dead end it exists to stop.
pub fn stored_field_names(compiled: &Value) -> BTreeSet<String> {
    compiled
        .get("fields")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

impl Ready<'_> {
    /// One contact's HTML and plain-text parts, both filled from the one stored AST.
    ///
    /// A send rendition's message must carry this recipient's `{public_url}/u/{token}` in BOTH
    /// parts, or it is refused. The last line of defence (review C1, I1, I2): it holds whatever
    /// happened to the frozen document or to the parts built from it.
    pub fn render(&self, contact: &Contact<'_>) -> Result<Rendered, StoredError> {
        if self.0.unsubscribe.is_some() && contact.unsubscribe_token.is_empty() {
            return Err(StoredError::NoUnsubscribeToken);
        }
        // `Some("")` would write `{base}/o/` and `{base}/l//{i}`; `None` is the untracked case.
        if contact.tracking_token == Some("") {
            return Err(StoredError::NoTrackingToken);
        }
        // The click route's base is the rendition's own frozen unsubscribe prefix, so it cannot
        // drift from the footer's (M2d spec §4.3.3).
        let links = self
            .0
            .unsubscribe
            .as_deref()
            .and_then(|p| p.strip_suffix("/u/"))
            .map(|b| (b, self.0.links.as_slice()));
        let html = render::substitute(&self.0.html, &self.0.nodes, contact, links)
            .map_err(StoredError::Render)?;
        let text = match &self.0.text {
            Some(t) => text::render_text(t, contact).map_err(StoredError::Render)?,
            // THE GUARD. `text_from_html` takes any HTML and has no way to tell the marked
            // document from the substituted one; handed the marked one it produces a text part
            // full of `~nmmt~3~tmmn~`. `html` above is the output of `substitute`, and this is
            // the only call site in the product that could ever pass the other.
            //
            // shortcut: the check is for the marker text, so a contact whose own stored value
            // spells `~nmmt~` makes that one message unsendable with a named error. Fail closed
            // and loud beats delivering marker text; tighten it if a real value ever trips it.
            None if html.contains(render::MARK_START) => return Err(StoredError::MarkedText),
            None => text::text_from_html(&html),
        };
        if let Some(prefix) = &self.0.unsubscribe {
            let (in_text, in_html) = render::footer_link(prefix, contact.unsubscribe_token);
            if !render::carries_link(&html, &in_html) {
                return Err(StoredError::FooterLost("HTML"));
            }
            if !render::carries_link(&text, &in_text) {
                return Err(StoredError::FooterLost("plain-text"));
            }
        }
        Ok(Rendered { html, text })
    }
}

/// Compile BOTH renditions of one template, in one call, from one `Registry`.
///
/// **This is R5's whole assumption and only this function keeps it.** The HTML part and the
/// plain-text part cannot disagree only while both are compiled at the same instant against the
/// same field definitions. Compiled separately, a field archived between the two calls makes a tag
/// fail in one part while the other still renders it — the two halves of one message disagreeing
/// about the recipient's name, with nothing to notice. The `Registry` is built here, from the
/// `defs` passed in, so there is one of it by construction.
///
/// The caller writes the result in ONE transaction, and invalidates the pair together.
/// `keep_archived` is the set of custom fields the STORED version of this document already named
/// (see [`stored_field_names`]). Those may still be named although they are archived; a reference
/// that is NEW in `authored` is still refused. Empty for a template that has no stored version.
pub fn compile(
    authored: Authored<'_>,
    brand: &BrandKit,
    defs: &[Definition],
    keep_archived: &BTreeSet<String>,
) -> Result<Stored, CompileError> {
    build(authored, brand, defs, keep_archived, None)
}

/// [`compile`], plus the system footer: the rendition a campaign sends (spec §4.5 ruling 4).
///
/// The footer's HTML goes in after `mrml` and before classification, at the operator document's
/// real end of body, so it is part of the classified document. It carries the unsubscribe link
/// `{public_url}/u/{token}` and, for a raw `mjml` or `html` template, the postal address (issue
/// 51); a `blocks` template's plain-text part gains the same link. Never stored in
/// `templates.compiled`: a template preview shows the operator's document, not the send.
pub fn compile_for_send(
    authored: Authored<'_>,
    brand: &BrandKit,
    defs: &[Definition],
    keep_archived: &BTreeSet<String>,
    footer: &SystemFooter<'_>,
) -> Result<Stored, CompileError> {
    build(authored, brand, defs, keep_archived, Some(footer))
}

/// The rendition a campaign launches with: always a fresh [`compile_for_send`], against the kit
/// and the fields as they are now (issue 49). `stored` is the template's `templates.compiled`,
/// read only to refuse field drift and to know which archived fields the template may keep
/// naming. Every other way that column can be unusable — stale, another marker format,
/// unreadable, absent — is cured by the compile this does anyway.
///
/// Runs `mrml`: call it in `spawn_blocking`, outside any transaction (spec §4.5 ruling 3).
pub fn freeze(
    stored: Option<&Value>,
    authored: Authored<'_>,
    brand: &BrandKit,
    defs: &[Definition],
    footer: &SystemFooter<'_>,
) -> Result<Stored, FreezeError> {
    if let Some(Ok(s)) = stored.map(Stored::from_column)
        && let Err(StoredError::FieldsChanged(drift)) = s.ready(&fingerprint(authored, brand), defs)
    {
        return Err(FreezeError::Drift(drift));
    }
    let keep = stored.map(stored_field_names).unwrap_or_default();
    compile_for_send(authored, brand, defs, &keep, footer).map_err(FreezeError::Compile)
}

fn build(
    authored: Authored<'_>,
    brand: &BrandKit,
    defs: &[Definition],
    keep_archived: &BTreeSet<String>,
    footer: Option<&SystemFooter<'_>>,
) -> Result<Stored, CompileError> {
    let registry =
        tag::Registry::from_definitions(defs).allowing(keep_archived.iter().map(String::as_str));
    // A `blocks` template already emits the postal address from the kit; once is enough.
    let html_footer = footer.map(|f| f.html(!matches!(authored, Authored::Blocks(_))));
    let compiled = mjml::compile_authored(authored, brand, &registry, html_footer.as_deref())?;
    let text = match authored {
        Authored::Blocks(blocks) => {
            let t = text::compile_text(blocks, brand, &registry)?;
            Some(match footer {
                Some(f) => text::with_footer(t, f.base())?,
                None => t,
            })
        }
        // A raw template has no block tree, so there is no plain-text skeleton to compile: its
        // text part comes from the substituted HTML at send (see `Ready::render`), footer and all.
        Authored::Mjml(_) | Authored::Html(_) => None,
    };
    Ok(Stored {
        marker_format_version: MARKER_FORMAT_VERSION,
        fingerprint: fingerprint(authored, brand),
        fields: field_kinds(&compiled.nodes, defs),
        unsubscribe: footer.map(|f| format!("{}/u/", f.base())),
        // Built from the document just classified, by the predicate a render rewrites with.
        links: footer
            .map(|f| render::tracked_links(&compiled.html, f.base(), f.prior_links))
            .unwrap_or_default(),
        nodes: compiled.nodes,
        html: compiled.html,
        text,
    })
}

/// The key that invalidates a stored rendition: the authored document, the brand kit, the marker
/// format and the versions of the two crates whose output is stored.
///
/// A brand-kit edit alone changes this for a `blocks` template, which is the point — the kit is
/// emitted into the MJML the classifier ran over, so a kit change makes the stored classification
/// describe a document that is no longer produced. A raw template is compiled without the kit, so
/// the kit is not part of its key.
pub fn fingerprint(authored: Authored<'_>, brand: &BrandKit) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut field = |bytes: &[u8]| {
        buf.extend_from_slice(bytes);
        // U+0000 as a terminator, which is unambiguous only because no part can contain one:
        // Postgres `text` and `jsonb` both refuse it, so neither a raw source nor a brand-kit
        // string can carry one into this buffer. It is NOT true that every part is JSON or a
        // number — the raw `source` and the kit's strings are operator text — so if a part ever
        // could hold a NUL, `("a\0b", "c")` and `("a", "b\0c")` would hash identically and two
        // different documents would share one key.
        buf.push(0);
    };
    field(MARKER_FORMAT_VERSION.to_string().as_bytes());
    field(MRML_VERSION.as_bytes());
    field(URL_VERSION.as_bytes());
    match authored {
        // `Block` and `Run` hold no maps, so serde's output is ordered by declaration and is
        // stable for a given tree.
        Authored::Blocks(blocks) => {
            field(b"blocks");
            // `expect`, not `unwrap_or_default`: a serialise failure there would collapse every
            // tree to the same key, which is the one outcome this function must never produce.
            // `Block` and `Run` hold no map and no non-finite number, so it cannot fail.
            field(&serde_json::to_vec(blocks).expect("a block tree always serialises"));
        }
        Authored::Mjml(source) => {
            field(b"mjml");
            field(source.as_bytes());
        }
        Authored::Html(source) => {
            field(b"html");
            field(source.as_bytes());
        }
    }
    // The renderer's view of the kit, not the stored row: an unknown key the emitter ignores
    // cannot change the rendered document, so it must not invalidate it either. Only a `blocks`
    // template reads the kit; for a raw one a kit save would be a stale key for no reason.
    if matches!(authored, Authored::Blocks(_)) {
        for part in [
            &brand.logo_url,
            &brand.background,
            &brand.text,
            &brand.accent,
            &brand.accent_text,
            &brand.font_stack,
            &brand.footer_text,
            &brand.footer_address,
        ] {
            field(part.as_deref().unwrap_or("").as_bytes());
        }
    }
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The custom fields an AST names, and the kind each has now. A system field (`email`,
/// `first_name`, …) has no definition and is not recorded: nothing can change it. Nor is either of
/// the footer's slots, even when a custom field is keyed like one (issues/44): that field is not
/// what fills it, and its kind changing must not halt a campaign that is sending (review M4).
fn field_kinds(nodes: &[Node], defs: &[Definition]) -> BTreeMap<String, FieldKind> {
    let mut out = BTreeMap::new();
    let mut pending: Vec<&[Node]> = vec![nodes];
    while let Some(level) = pending.pop() {
        for node in level {
            let key = match node {
                Node::Text(_) => continue,
                Node::Field { key, .. } => key,
                Node::Present { key, body } | Node::Absent { key, body } => {
                    pending.push(body);
                    key
                }
                Node::Equals { key, body, .. } => {
                    pending.push(body);
                    key
                }
            };
            if key == UNSUBSCRIBE_TOKEN || key == TRACKING_TOKEN {
                continue;
            }
            if let Some(d) = defs.iter().find(|d| &d.key == key) {
                out.insert(key.clone(), d.kind);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::block::Run;
    use serde_json::json;

    /// The two versions in [`MRML_VERSION`] and [`URL_VERSION`] are part of the key that
    /// invalidates every stored rendition. Held to the manifest, because the failure they exist
    /// to prevent is exactly the one a hand-written constant invites: a bump that leaves every
    /// stored document claiming to have been produced by the version before it.
    #[test]
    fn dependency_versions_match_cargo_toml() {
        let manifest = include_str!("../../Cargo.toml");
        for (crate_name, declared) in [("mrml", MRML_VERSION), ("url", URL_VERSION)] {
            let line = manifest
                .lines()
                .find(|l| l.starts_with(&format!("{crate_name} = ")))
                .unwrap_or_else(|| panic!("Cargo.toml declares {crate_name} on a line of its own"));
            assert!(
                line.contains(&format!("\"{declared}\"")),
                "{crate_name} is {line:?} in Cargo.toml but this module says {declared}: bump the \
                 constant, which invalidates every stored rendition on purpose"
            );
        }
    }

    fn defs() -> Vec<Definition> {
        vec![Definition {
            id: uuid::Uuid::nil(),
            key: "colour".into(),
            label: "Colour".into(),
            kind: FieldKind::Text,
            options: vec![],
            archived_at: None,
        }]
    }

    fn blocks() -> Vec<Block> {
        vec![Block::Text {
            runs: vec![Run::Tag {
                source: "{{ colour }}".into(),
            }],
        }]
    }

    /// The key that invalidates every stored rendition, pinned to one digest.
    ///
    /// Nothing else can hold this. `MRML_VERSION` and `URL_VERSION` are compile-time constants,
    /// so no test can vary one and watch the key move; without a golden value, dropping either
    /// from the hash changes nothing any test can see — and a stored classification that outlives
    /// an `mrml` bump is a security bug, not a cache miss.
    ///
    /// **When this fails, that is the signal, not the noise.** Either a dependency was bumped —
    /// in which case every stored rendition in every database is now stale on purpose and the new
    /// digest goes here — or the key's own layout changed, which has the same effect and wants
    /// the same deliberate update.
    #[test]
    fn the_fingerprint_is_the_key_it_claims_to_be() {
        let blocks = blocks();
        assert_eq!(
            fingerprint(Authored::Blocks(&blocks), &BrandKit::default()),
            "c2ac28e823221a1766216d3078acd92f2de1abe269ecfc150affe44f4acf21da",
            "mrml {MRML_VERSION}, url {URL_VERSION}, marker format {MARKER_FORMAT_VERSION}"
        );
    }

    #[test]
    fn the_fingerprint_moves_when_the_brand_kit_alone_moves() {
        let b = blocks();
        let plain = BrandKit::default();
        let painted = BrandKit {
            accent: Some("#ff0000".into()),
            ..BrandKit::default()
        };
        assert_ne!(
            fingerprint(Authored::Blocks(&b), &plain),
            fingerprint(Authored::Blocks(&b), &painted)
        );
        // An unknown key cannot reach `BrandKit`, so it cannot invalidate anything either.
        assert_eq!(
            fingerprint(Authored::Blocks(&b), &plain),
            fingerprint(Authored::Blocks(&b), &BrandKit::default())
        );
    }

    #[test]
    fn a_raw_template_does_not_depend_on_the_brand_kit() {
        let painted = BrandKit {
            accent: Some("#ff0000".into()),
            footer_address: Some("1 Main St".into()),
            ..BrandKit::default()
        };
        for authored in [Authored::Mjml("<mjml></mjml>"), Authored::Html("<p>x</p>")] {
            assert_eq!(
                fingerprint(authored, &BrandKit::default()),
                fingerprint(authored, &painted)
            );
        }
    }

    #[test]
    fn a_stored_rendition_is_refused_when_the_marker_format_is_not_ours() {
        let stored = compile(
            Authored::Blocks(&blocks()),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        let mut v = serde_json::to_value(&stored).expect("serialises");
        v["marker_format_version"] = json!(MARKER_FORMAT_VERSION + 1);
        assert_eq!(
            Stored::from_column(&v),
            Err(StoredError::MarkerFormat {
                stored: MARKER_FORMAT_VERSION + 1
            })
        );
    }

    #[test]
    fn an_over_deep_stored_ast_is_refused_although_serde_accepted_it() {
        let stored = compile(
            Authored::Blocks(&blocks()),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        let mut v = serde_json::to_value(&stored).expect("serialises");
        let mut deep = json!([]);
        for _ in 0..=tag::MAX_DEPTH {
            deep = json!([{ "Present": { "key": "colour", "body": deep } }]);
        }
        v["nodes"] = deep;
        assert_eq!(Stored::from_column(&v), Err(StoredError::TooDeep));
    }

    #[test]
    fn a_purge_or_a_kind_change_is_drift_but_an_archive_is_not() {
        let stored = compile(
            Authored::Blocks(&blocks()),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        let key = fingerprint(Authored::Blocks(&blocks()), &BrandKit::default());
        assert!(stored.ready(&key, &defs()).is_ok());

        let mut archived = defs();
        archived[0].archived_at = Some(chrono::Utc::now());
        assert!(
            stored.ready(&key, &archived).is_ok(),
            "archiving is not drift"
        );

        let mut retyped = defs();
        retyped[0].kind = FieldKind::Select;
        assert_eq!(
            stored.ready(&key, &retyped).unwrap_err(),
            StoredError::FieldsChanged(vec![Drift {
                key: "colour".into(),
                was: FieldKind::Text,
                now: Some(FieldKind::Select),
            }])
        );
        assert_eq!(
            stored.ready(&key, &[]).unwrap_err(),
            StoredError::FieldsChanged(vec![Drift {
                key: "colour".into(),
                was: FieldKind::Text,
                now: None,
            }])
        );
    }

    #[test]
    fn a_stale_fingerprint_is_refused_rather_than_rendered() {
        let stored = compile(
            Authored::Blocks(&blocks()),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        let moved = fingerprint(
            Authored::Blocks(&blocks()),
            &BrandKit {
                accent: Some("#ff0000".into()),
                ..BrandKit::default()
            },
        );
        assert_eq!(
            stored.ready(&moved, &defs()).unwrap_err(),
            StoredError::Stale
        );
    }

    #[test]
    fn a_raw_html_template_keeps_its_text_part_free_of_markers() {
        let stored = compile(
            Authored::Html("<p>Hi {{ colour }}</p>"),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        assert!(stored.html.contains(render::MARK_START), "stored marked");
        let key = fingerprint(
            Authored::Html("<p>Hi {{ colour }}</p>"),
            &BrandKit::default(),
        );
        let attrs = json!({ "colour": "teal" });
        let out = stored
            .ready(&key, &defs())
            .expect("ready")
            .render(&Contact {
                email: "a@b.example",
                first_name: "A",
                last_name: "B",
                timezone: None,
                attributes: &attrs,
                unsubscribe_token: "TOK",
                tracking_token: None,
            })
            .expect("renders");
        assert!(!out.text.contains(render::MARK_START), "{}", out.text);
        assert!(out.text.contains("teal"), "{}", out.text);
    }

    // -----------------------------------------------------------------------------------------
    // The send rendition (M2c R2: issues 48, 49 and 51)

    const ADDRESS: &str = "1 Main St\nTown";
    const LINK: &str = "https://mail.example.test/u/";

    fn kit_with_address() -> BrandKit {
        BrandKit {
            footer_address: Some(ADDRESS.into()),
            ..BrandKit::default()
        }
    }

    fn public_url() -> url::Url {
        "https://mail.example.test/".parse().expect("a url")
    }

    fn send_compile(authored: Authored<'_>, defs: &[Definition]) -> Result<Stored, CompileError> {
        let url = public_url();
        let footer = SystemFooter {
            public_url: &url,
            postal_address: ADDRESS,
            prior_links: &[],
        };
        compile_for_send(
            authored,
            &kit_with_address(),
            defs,
            &BTreeSet::new(),
            &footer,
        )
    }

    /// Compile for send, then one recipient's copy, used with the rendition's OWN key as a frozen
    /// campaign rendition is.
    fn send(authored: Authored<'_>, token: &str) -> Result<Rendered, CompileError> {
        let stored = send_compile(authored, &defs())?;
        let attrs = json!({ "colour": "teal" });
        Ok(stored
            .ready(stored.fingerprint(), &defs())
            .expect("ready")
            .render(&Contact {
                email: "a@b.example",
                first_name: "Ada",
                last_name: "B",
                timezone: None,
                attributes: &attrs,
                unsubscribe_token: token,
                tracking_token: None,
            })
            .expect("renders"))
    }

    /// What follows the unsubscribe base in `s`, up to the first of `stop`.
    fn token_part<'s>(s: &'s str, stop: &[char]) -> &'s str {
        let at = s.find(LINK).unwrap_or_else(|| panic!("no link in {s}")) + LINK.len();
        let rest = &s[at..];
        &rest[..rest.find(stop).unwrap_or(rest.len())]
    }

    #[test]
    fn a_raw_html_send_rendition_carries_the_postal_address_and_an_absolute_unsubscribe_link() {
        let out = send(
            Authored::Html("<html><body><p>Hi {{ first_name }}</p></body></html>"),
            "TOK",
        )
        .expect("compiles");
        assert!(out.html.contains("1 Main St<br>Town"), "{}", out.html);
        assert!(
            out.html
                .contains(r#"href="https://mail.example.test/u/TOK""#),
            "{}",
            out.html
        );
        // The operator's tag keeps its own slot; the footer's slot comes after it.
        assert!(out.html.contains("<p>Hi Ada</p>"), "{}", out.html);
        assert!(
            out.text.contains("https://mail.example.test/u/TOK"),
            "{}",
            out.text
        );
        assert!(out.text.contains("1 Main St"), "{}", out.text);
    }

    #[test]
    fn a_raw_mjml_send_rendition_carries_both() {
        let out = send(
            Authored::Mjml(
                "<mjml><mj-body><mj-section><mj-column><mj-text>Hi {{ first_name }}</mj-text>\
                 </mj-column></mj-section></mj-body></mjml>",
            ),
            "TOK",
        )
        .expect("compiles");
        assert!(out.html.contains("1 Main St<br>Town"), "{}", out.html);
        assert!(
            out.html
                .contains(r#"href="https://mail.example.test/u/TOK""#),
            "{}",
            out.html
        );
        assert!(
            out.text.contains("https://mail.example.test/u/TOK"),
            "{}",
            out.text
        );
        assert!(out.text.contains("1 Main St"), "{}", out.text);
    }

    #[test]
    fn a_blocks_send_rendition_has_the_address_once_and_the_link() {
        let out = send(Authored::Blocks(&blocks()), "TOK").expect("compiles");
        assert_eq!(out.html.matches("1 Main St").count(), 1, "{}", out.html);
        assert!(
            out.html
                .contains(r#"href="https://mail.example.test/u/TOK""#),
            "{}",
            out.html
        );
        assert_eq!(out.text.matches("1 Main St").count(), 1, "{}", out.text);
        assert!(
            out.text
                .ends_with("\n\nUnsubscribe: https://mail.example.test/u/TOK"),
            "{}",
            out.text
        );
    }

    #[test]
    fn the_footer_sits_before_the_real_end_of_body_not_a_commented_one() {
        let out = send(
            Authored::Html("<html><body><p>x</p><!-- </body> --></body></html>"),
            "TOK",
        )
        .expect("compiles");
        let comment = out.html.find("<!-- </body> -->").expect("the comment");
        let footer = out.html.find("<table role=").expect("the footer");
        assert!(comment < footer, "{}", out.html);
        assert!(
            out.html
                .ends_with(r#"border:0;"></td></tr></table></body></html>"#),
            "{}",
            out.html
        );
        // No end of body at all: the footer goes at the end.
        let out = send(Authored::Html("<p>x</p>"), "TOK").expect("compiles");
        assert!(
            out.html.ends_with(r#"border:0;"></td></tr></table>"#),
            "{}",
            out.html
        );
    }

    /// Each of these ends somewhere the footer would be text, not markup. The classifier alone
    /// would pass the title, attribute and plaintext cases: the footer's slot there is text or a
    /// URL to it, and only a browser shows that nothing is rendered.
    #[test]
    fn a_document_ending_inside_an_unclosed_comment_or_script_is_footer_hidden() {
        for source in [
            "<p>x</p><!-- unclosed",
            "<p>x</p><script>var a = 1;",
            "<p>x</p><script>var a = '</body>';",
            "<p>x</p><style>p { color: red }",
            "<p>x</p><title>x",
            "<p>x</p><a href=\"https://x.example/",
            "<body><p>x</p><plaintext></body>",
            // The operator's own copy of the probe character is not a probe.
            "<p>\u{E00F}</p><title>x",
        ] {
            assert_eq!(
                send_compile(Authored::Html(source), &defs()).unwrap_err(),
                CompileError::FooterHidden,
                "{source}"
            );
        }
    }

    #[test]
    fn an_operator_cannot_name_the_unsubscribe_slot() {
        // Not even through a custom field keyed the same (issues/44: keys are not reserved).
        let mut defs = defs();
        defs.push(Definition {
            key: UNSUBSCRIBE_TOKEN.into(),
            ..defs[0].clone()
        });
        let tag = format!("{{{{ {UNSUBSCRIBE_TOKEN} }}}}");
        let html = format!("<a href=\"https://x.example/{tag}\">x</a>");
        let blocks = vec![Block::Text {
            runs: vec![Run::Tag {
                source: tag.clone(),
            }],
        }];
        let refused = CompileError::Tag(tag::TagError::UnknownField(UNSUBSCRIBE_TOKEN.into()));
        for authored in [Authored::Html(&html), Authored::Blocks(&blocks)] {
            assert_eq!(send_compile(authored, &defs).unwrap_err(), refused);
            assert_eq!(
                compile(authored, &BrandKit::default(), &defs, &BTreeSet::new()).unwrap_err(),
                refused
            );
        }
    }

    /// A merge tag cannot be written that names the slot, so the other way to reach it is a marker
    /// the classifier reads DECODED: `~` as `&#126;`. Index 1 is the footer's here.
    #[test]
    fn a_character_reference_cannot_forge_the_unsubscribe_slot() {
        let forged = "&#126;nmmt&#126;1&#126;tmmn&#126;";
        for source in [
            format!("<p>{{{{ first_name }}}}</p><a href=\"https://evil.example/{forged}\">x</a>"),
            format!("<p>{{{{ first_name }}}}</p><p>{forged}</p>"),
            // RCDATA decodes character references too, where raw text does not.
            format!("<p>{{{{ first_name }}}}</p><textarea>{forged}</textarea>"),
            format!("<title>{forged}</title><p>{{{{ first_name }}}}</p>"),
        ] {
            assert_eq!(
                send_compile(Authored::Html(&source), &defs()).unwrap_err(),
                CompileError::ForgedMarker,
                "{source}"
            );
        }
    }

    /// Review C1. The footer goes in before the real `</body>`, and an operator conditional that
    /// opens before it and closes after it would rewind the footer away for every recipient the
    /// branch is not taken for — here, all of them. An ALIAS of the operator's own conditional,
    /// written with character references, is the same thing by another route.
    #[test]
    fn an_operator_conditional_cannot_wrap_the_footer() {
        let open = "&#126;nmmt&#126;0o&#126;tmmn&#126;";
        let close = "&#126;nmmt&#126;0c&#126;tmmn&#126;";
        for source in [
            "<html><body><p>Hi</p>{{^ email }}</body></html>{{/ email }}".to_owned(),
            "<html><body><p>Hi</p>{{# colour }}</body>{{/ colour }}</html>".to_owned(),
            "<p>Hi</p>{{^ email }}</body>{{/ email }}".to_owned(),
            format!(
                "<html><body><p>{{{{^ email }}}}x{{{{/ email }}}}</p>{open}</body></html>{close}"
            ),
        ] {
            let refused = send_compile(Authored::Html(&source), &defs())
                .err()
                .unwrap_or_else(|| panic!("froze with the footer inside a conditional: {source}"));
            assert_eq!(
                refused,
                CompileError::Render(render::RenderError::FooterInBlock),
                "{source}"
            );
        }
    }

    /// Review I1. A raw template's text part comes from its HTML, and a `<head>` left open must
    /// not swallow the body — the footer with it.
    #[test]
    fn an_open_head_does_not_lose_the_footer_from_the_text_part() {
        for source in [
            "<html><body><p>Hi</p><head></body></html>",
            "<p>Hi</p><head>",
            "<html><head><p>Hi</p></html>",
        ] {
            let out = send(Authored::Html(source), "TOK").expect("compiles");
            assert!(out.text.contains("Hi"), "{source}: {:?}", out.text);
            assert!(
                out.text.contains("https://mail.example.test/u/TOK"),
                "{source}: {:?}",
                out.text
            );
            assert!(out.text.contains("1 Main St"), "{source}: {:?}", out.text);
        }
    }

    /// Review judged item 1: the last line. Whatever happened to the frozen document, a message
    /// whose HTML or text part does not carry this recipient's link is not built. Planted here by
    /// editing the frozen column so an operator conditional wraps the footer — which the freeze
    /// would refuse, so this holds the render guard on its own.
    #[test]
    fn a_message_that_lost_its_footer_is_not_rendered() {
        let stored = send_compile(Authored::Html("<p>{{^ email }}x{{/ email }}</p>"), &defs())
            .expect("compiles");
        let mut column = serde_json::to_value(&stored).expect("serialises");
        let close = format!("{}0c{}", render::MARK_START, render::MARK_END);
        let html = column["html"].as_str().expect("html").replace(&close, "") + &close;
        column["html"] = json!(html);
        let tampered = Stored::from_column(&column).expect("reads");
        let attrs = json!({});
        let out = tampered
            .ready(tampered.fingerprint(), &defs())
            .expect("ready")
            .render(&Contact {
                email: "a@b.example",
                first_name: "Ada",
                last_name: "B",
                timezone: None,
                attributes: &attrs,
                unsubscribe_token: "TOK",
                tracking_token: None,
            });
        assert_eq!(out.unwrap_err(), StoredError::FooterLost("HTML"));
    }

    /// R3 carry. A token that is the front of another recipient's token must not pass on that
    /// other link: the stored footer is edited so its link runs on past the slot (`…/u/TOK` +
    /// `EN2`), which is what a message for `TOKEN2` looks like to a check that only asks whether
    /// `…/u/TOK` is somewhere in the part.
    #[test]
    fn a_token_that_is_the_front_of_another_tokens_link_is_not_the_link() {
        let stored = send_compile(Authored::Html("<p>x</p>"), &defs()).expect("compiles");
        let mut column = serde_json::to_value(&stored).expect("serialises");
        let html = column["html"].as_str().expect("html");
        // The unsubscribe slot's end; the pixel's is followed by ` width=`.
        let slot_end = format!("{}\" style=", render::MARK_END);
        assert_eq!(
            html.matches(&slot_end).count(),
            1,
            "one link slot, then a quote"
        );
        column["html"] =
            json!(html.replace(&slot_end, &format!("{}EN2\" style=", render::MARK_END)));
        let tampered = Stored::from_column(&column).expect("reads");
        let attrs = json!({});
        let render_for = |token: &str| {
            tampered
                .ready(tampered.fingerprint(), &defs())
                .expect("ready")
                .render(&Contact {
                    email: "a@b.example",
                    first_name: "Ada",
                    last_name: "B",
                    timezone: None,
                    attributes: &attrs,
                    unsubscribe_token: token,
                    tracking_token: None,
                })
        };
        // The link in the part is `…/u/TOKEN2`: it is TOKEN2's, so it is not TOK's.
        assert_eq!(
            render_for("TOK").unwrap_err(),
            StoredError::FooterLost("HTML")
        );
        // Control: the rendition as compiled builds for the same token, so the refusal above is
        // about the run-on and not about the token.
        stored
            .ready(stored.fingerprint(), &defs())
            .expect("ready")
            .render(&Contact {
                email: "a@b.example",
                first_name: "Ada",
                last_name: "B",
                timezone: None,
                attributes: &attrs,
                unsubscribe_token: "TOK",
                tracking_token: None,
            })
            .expect("the untampered message is built");
    }

    #[test]
    fn a_link_ends_at_a_character_no_token_is_made_of() {
        for (part, link, ok) in [
            ("<a href=\"https://x/u/TOK\">", "https://x/u/TOK", true),
            ("Unsubscribe: https://x/u/TOK", "https://x/u/TOK", true),
            ("(https://x/u/TOK)", "https://x/u/TOK", true),
            ("https://x/u/TOKEN2", "https://x/u/TOK", false),
            ("https://x/u/TOK.sig", "https://x/u/TOK", false),
            ("https://x/u/TOK%20", "https://x/u/TOK", false),
            // The first match is a prefix of another link, the second is the real one.
            (
                "https://x/u/TOKEN2 and https://x/u/TOK",
                "https://x/u/TOK",
                true,
            ),
        ] {
            assert_eq!(render::carries_link(part, link), ok, "{part}");
        }
    }

    /// Review I2. An empty token is a dead link sent to every recipient without a word.
    #[test]
    fn an_empty_unsubscribe_token_is_refused() {
        let stored = send_compile(Authored::Html("<p>x</p>"), &defs()).expect("compiles");
        let attrs = json!({});
        let out = stored
            .ready(stored.fingerprint(), &defs())
            .expect("ready")
            .render(&Contact {
                email: "a@b.example",
                first_name: "Ada",
                last_name: "B",
                timezone: None,
                attributes: &attrs,
                unsubscribe_token: "",
                tracking_token: None,
            });
        assert_eq!(out.unwrap_err(), StoredError::NoUnsubscribeToken);
    }

    /// S2 review: an empty tracking token is a dead pixel and dead links, refused like an empty
    /// unsubscribe token.
    #[test]
    fn an_empty_tracking_token_is_refused() {
        let stored = send_compile(Authored::Html("<p>x</p>"), &defs()).expect("compiles");
        let attrs = json!({});
        let ready = stored.ready(stored.fingerprint(), &defs()).expect("ready");
        let contact = |tracking_token| Contact {
            email: "a@b.example",
            first_name: "Ada",
            last_name: "B",
            timezone: None,
            attributes: &attrs,
            unsubscribe_token: "TOK",
            tracking_token,
        };
        assert_eq!(
            ready.render(&contact(Some(""))).unwrap_err(),
            StoredError::NoTrackingToken
        );
        assert!(ready.render(&contact(Some("TRK"))).is_ok());
    }

    /// Review M4. The footer's slot is not a custom field, whatever a custom field is keyed, so a
    /// change to such a field must not halt a campaign that is sending.
    #[test]
    fn a_custom_field_keyed_like_the_slot_is_not_recorded_in_a_send_rendition() {
        let mut defs = defs();
        defs.push(Definition {
            key: UNSUBSCRIBE_TOKEN.into(),
            ..defs[0].clone()
        });
        let stored = send_compile(Authored::Html("<p>x</p>"), &defs).expect("compiles");
        assert_eq!(stored.field_names().count(), 0);
        defs[1].kind = FieldKind::Select;
        assert!(stored.ready(stored.fingerprint(), &defs).is_ok());
    }

    #[test]
    fn a_token_cannot_leave_its_path_component() {
        let token = "a/../b?x=1#y";
        for authored in [Authored::Html("<p>x</p>"), Authored::Blocks(&blocks())] {
            let out = send(authored, token).expect("compiles");
            for (part, stop) in [(&out.html, &['"'][..]), (&out.text, &[')', '\n', ' '][..])] {
                let path = token_part(part, stop);
                assert!(
                    !path.contains(['/', '?', '#']),
                    "the token left its path segment as {path:?}: {part}"
                );
            }
        }
    }

    /// A contact's own data can hold any key (an import, a custom field keyed like the slot), and
    /// the slot is answered before it is looked at.
    #[test]
    fn a_contact_value_cannot_fill_the_unsubscribe_slot() {
        let stored = send_compile(Authored::Html("<p>x</p>"), &defs()).expect("compiles");
        let attrs = json!({ UNSUBSCRIBE_TOKEN: "EVIL" });
        let out = stored
            .ready(stored.fingerprint(), &defs())
            .expect("ready")
            .render(&Contact {
                email: "a@b.example",
                first_name: "Ada",
                last_name: "B",
                timezone: None,
                attributes: &attrs,
                unsubscribe_token: "TOK",
                tracking_token: None,
            })
            .expect("renders");
        assert_eq!(token_part(&out.html, &['"']), "TOK", "{}", out.html);
    }

    #[test]
    fn the_postal_address_is_text_not_markup() {
        let url = public_url();
        let footer = SystemFooter {
            public_url: &url,
            postal_address: "<b>A&B</b> {{ email }}",
            prior_links: &[],
        };
        let stored = compile_for_send(
            Authored::Html("<p>x</p>"),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
            &footer,
        )
        .expect("an address cannot name a tag or open an element");
        // Escaped as text, `{` included, so `{{ email }}` is what the recipient reads.
        assert!(
            stored
                .html
                .contains("&lt;b&gt;A&amp;B&lt;/b&gt; &#123;&#123; email }}<br>"),
            "{}",
            stored.html
        );
    }

    fn button_blocks() -> Vec<Block> {
        vec![
            blocks().remove(0),
            Block::Button {
                label: "Go".into(),
                href: "https://x.example/".into(),
            },
        ]
    }

    #[test]
    fn a_brand_kit_edit_does_not_stop_a_freeze() {
        let b = button_blocks();
        let saved = compile(
            Authored::Blocks(&b),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        assert!(saved.html.contains("#2563eb"), "the default accent");
        let column = serde_json::to_value(&saved).expect("serialises");
        let painted = BrandKit {
            accent: Some("#ff0000".into()),
            ..kit_with_address()
        };
        let url = public_url();
        let footer = SystemFooter {
            public_url: &url,
            postal_address: ADDRESS,
            prior_links: &[],
        };
        let frozen = freeze(
            Some(&column),
            Authored::Blocks(&b),
            &painted,
            &defs(),
            &footer,
        )
        .expect("a kit edit is cured by the compile a freeze makes anyway");
        assert!(frozen.html.contains("#ff0000"), "{}", frozen.html);
        assert!(frozen.html.contains(LINK), "{}", frozen.html);
    }

    /// With the kit moved TOO: a stale rendition is the case a freeze recompiles, so drift has to
    /// be answered before staleness or the recompile re-reads every tag against the new kinds.
    #[test]
    fn field_drift_still_refuses_a_freeze() {
        let b = button_blocks();
        let saved = compile(
            Authored::Blocks(&b),
            &BrandKit::default(),
            &defs(),
            &BTreeSet::new(),
        )
        .expect("compiles");
        let column = serde_json::to_value(&saved).expect("serialises");
        let mut retyped = defs();
        retyped[0].kind = FieldKind::Select;
        let painted = BrandKit {
            accent: Some("#ff0000".into()),
            ..kit_with_address()
        };
        let url = public_url();
        let footer = SystemFooter {
            public_url: &url,
            postal_address: ADDRESS,
            prior_links: &[],
        };
        assert_eq!(
            freeze(
                Some(&column),
                Authored::Blocks(&b),
                &painted,
                &retyped,
                &footer
            )
            .unwrap_err(),
            FreezeError::Drift(vec![Drift {
                key: "colour".into(),
                was: FieldKind::Text,
                now: Some(FieldKind::Select),
            }])
        );
    }

    // -----------------------------------------------------------------------------------------
    // The tracked send rendition (M2d: spec §4.2.1–2, §4.3.1–4, §5.3)

    const TRK: &str = "TRK";

    /// [`defs`] plus a text field for each of `keys`.
    fn defs_with(keys: &[&str]) -> Vec<Definition> {
        let mut defs = defs();
        for key in keys {
            defs.push(Definition {
                key: (*key).into(),
                ..defs[0].clone()
            });
        }
        defs
    }

    /// A send rendition, frozen with `prior` as its variant's link table before this freeze.
    fn tracked_freeze(authored: Authored<'_>, defs: &[Definition], prior: &[String]) -> Stored {
        let url = public_url();
        let footer = SystemFooter {
            public_url: &url,
            postal_address: ADDRESS,
            prior_links: prior,
        };
        compile_for_send(
            authored,
            &kit_with_address(),
            defs,
            &BTreeSet::new(),
            &footer,
        )
        .expect("compiles")
    }

    /// Alice's copy, with `tracking` as the message's tracking token.
    fn tracked_render(
        stored: &Stored,
        defs: &[Definition],
        attrs: &Value,
        tracking: Option<&str>,
    ) -> Rendered {
        stored
            .ready(stored.fingerprint(), defs)
            .expect("ready")
            .render(&Contact {
                email: "alice+x@example.test",
                first_name: "Alice",
                last_name: "B",
                timezone: None,
                attributes: attrs,
                unsubscribe_token: "TOK",
                tracking_token: tracking,
            })
            .expect("renders")
    }

    /// Every value of `attr` in `html`, in document order. The serialiser writes every attribute
    /// in double quotes, so the next `"` ends one.
    fn attr_values<'h>(html: &'h str, attr: &str) -> Vec<&'h str> {
        let open = format!(" {attr}=\"");
        html.match_indices(&open)
            .map(|(at, _)| {
                let rest = &html[at + open.len()..];
                &rest[..rest.find('"').expect("a closing quote")]
            })
            .collect()
    }

    #[test]
    fn a_send_rendition_carries_the_pixel_right_after_the_unsubscribe_link() {
        for authored in [Authored::Html("<p>x</p>"), Authored::Blocks(&blocks())] {
            let stored = tracked_freeze(authored, &defs(), &[]);
            let out = tracked_render(&stored, &defs(), &json!({}), Some(TRK));
            assert!(
                out.html.contains(
                    r#"Unsubscribe</a><img src="https://mail.example.test/o/TRK" width="1" height="1" alt="""#
                ),
                "{}",
                out.html
            );
            // The plain-text part gains nothing.
            assert!(!out.text.contains("/o/"), "{}", out.text);
        }
    }

    #[test]
    fn a_message_without_a_tracking_token_has_a_test_pixel_and_untouched_links() {
        let stored = tracked_freeze(
            Authored::Html(r#"<a href="https://a.test/x">a</a>"#),
            &defs(),
            &[],
        );
        assert_eq!(stored.links(), ["https://a.test/x"]);
        let out = tracked_render(&stored, &defs(), &json!({}), None);
        assert!(
            out.html
                .contains(r#"<img src="https://mail.example.test/o/test""#),
            "{}",
            out.html
        );
        assert!(
            out.html.contains(r#"href="https://a.test/x""#),
            "{}",
            out.html
        );
        assert!(!out.html.contains("/l/"), "{}", out.html);
    }

    #[test]
    fn an_operator_cannot_name_the_tracking_slot() {
        // Not even through a custom field keyed the same (issues/44: keys are not reserved).
        let defs = defs_with(&[TRACKING_TOKEN]);
        let tag = format!("{{{{ {TRACKING_TOKEN} }}}}");
        let html = format!("<img src=\"https://x.example/{tag}\">");
        let blocks = vec![Block::Text {
            runs: vec![Run::Tag {
                source: tag.clone(),
            }],
        }];
        let refused = CompileError::Tag(tag::TagError::UnknownField(TRACKING_TOKEN.into()));
        for authored in [Authored::Html(&html), Authored::Blocks(&blocks)] {
            assert_eq!(send_compile(authored, &defs).unwrap_err(), refused);
            assert_eq!(
                compile(authored, &BrandKit::default(), &defs, &BTreeSet::new()).unwrap_err(),
                refused
            );
        }
    }

    #[test]
    fn a_custom_field_keyed_tracking_token_neither_fills_nor_drifts_the_slot() {
        let mut defs = defs_with(&[TRACKING_TOKEN]);
        let stored = tracked_freeze(Authored::Html("<p>x</p>"), &defs, &[]);
        assert_eq!(stored.field_names().count(), 0);
        let attrs = json!({ TRACKING_TOKEN: "EVIL" });
        for (tracking, want) in [(Some(TRK), TRK), (None, "test")] {
            let out = tracked_render(&stored, &defs, &attrs, tracking);
            assert!(
                out.html
                    .contains(&format!("src=\"https://mail.example.test/o/{want}\"")),
                "{}",
                out.html
            );
            assert!(!out.html.contains("EVIL"), "{}", out.html);
        }
        defs[1].kind = FieldKind::Select;
        assert!(stored.ready(stored.fingerprint(), &defs).is_ok());
    }

    #[test]
    fn http_links_are_tracked_in_first_appearance_order_and_nothing_else_is() {
        let defs = defs_with(&["website"]);
        let hrefs = [
            "https://a.test/x",
            "mailto:a@b.test",
            "tel:+100",
            "#top",
            "/relative",
            "https://mail.example.test/u/abc",
            "https://mail.example.test.evil.test/",
            "{{ website }}",
            "https://a.test/x",
            "http://b.test",
        ];
        let source: String = hrefs
            .iter()
            .map(|h| format!("<a href=\"{h}\">l</a>"))
            .collect();
        let stored = tracked_freeze(Authored::Html(&source), &defs, &[]);
        assert_eq!(
            stored.links(),
            [
                "https://a.test/x",
                "https://mail.example.test.evil.test/",
                "http://b.test/"
            ]
        );
        let attrs = json!({ "website": "https://w.test/" });
        let tracked = tracked_render(&stored, &defs, &attrs, Some(TRK));
        let plain = tracked_render(&stored, &defs, &attrs, None);
        let (tracked, plain) = (
            attr_values(&tracked.html, "href"),
            attr_values(&plain.html, "href"),
        );
        // The operator's ten, then the footer's unsubscribe link.
        assert_eq!((tracked.len(), plain.len()), (11, 11), "{tracked:?}");
        let l = |i: usize| format!("https://mail.example.test/l/TRK/{i}");
        let want = BTreeMap::from([(0, l(0)), (6, l(1)), (8, l(0)), (9, l(2))]);
        for (i, (t, p)) in tracked.iter().zip(&plain).enumerate() {
            match want.get(&i) {
                Some(w) => assert_eq!(t, w, "href {i}"),
                None => assert_eq!(t, p, "href {i}"),
            }
        }
    }

    #[test]
    fn the_unsubscribe_link_is_never_rewritten() {
        for authored in [Authored::Html("<p>x</p>"), Authored::Blocks(&blocks())] {
            let stored = tracked_freeze(authored, &defs(), &[]);
            let out = tracked_render(&stored, &defs(), &json!({}), Some(TRK));
            assert!(
                out.html
                    .contains(r#"href="https://mail.example.test/u/TOK""#),
                "{}",
                out.html
            );
            assert!(!out.html.contains("/l/"), "{}", out.html);
            assert!(!out.text.contains("/l/"), "{}", out.text);
        }
    }

    #[test]
    fn an_mj_button_link_is_tracked() {
        let source = "<mjml><mj-body><mj-section><mj-column>\
                      <mj-button href=\"https://a.test/buy\">Buy</mj-button>\
                      </mj-column></mj-section></mj-body></mjml>";
        let stored = tracked_freeze(Authored::Mjml(source), &defs(), &[]);
        assert_eq!(stored.links(), ["https://a.test/buy"]);
        let out = tracked_render(&stored, &defs(), &json!({}), Some(TRK));
        assert!(
            out.html
                .contains(r#"href="https://mail.example.test/l/TRK/0""#),
            "{}",
            out.html
        );
    }

    #[test]
    fn a_link_in_a_dropped_branch_keeps_every_later_index() {
        let defs = defs_with(&["vip"]);
        let stored = tracked_freeze(
            Authored::Html(
                "{{# vip }}<a href=\"https://a.test/vip\">v</a>{{/ vip }}\
                 <a href=\"https://b.test/\">b</a>",
            ),
            &defs,
            &[],
        );
        assert_eq!(stored.links(), ["https://a.test/vip", "https://b.test/"]);
        // Not a VIP: the branch, and its link, are dropped.
        let out = tracked_render(&stored, &defs, &json!({}), Some(TRK));
        assert!(!out.html.contains("a.test/vip"), "{}", out.html);
        assert_eq!(
            attr_values(&out.html, "href")[0],
            "https://mail.example.test/l/TRK/1",
            "{}",
            out.html
        );
    }

    #[test]
    fn a_link_built_from_a_merge_tag_is_neither_tracked_nor_stored() {
        let stored = tracked_freeze(
            Authored::Html("<a href=\"https://x.test/?e={{ email }}\">x</a>"),
            &defs(),
            &[],
        );
        assert!(stored.links().is_empty(), "{:?}", stored.links());
        let column = serde_json::to_value(&stored).expect("serialises");
        assert!(column.get("links").is_none(), "{column}");
        let out = tracked_render(&stored, &defs(), &json!({}), Some(TRK));
        assert!(!out.html.contains("/l/"), "{}", out.html);
        assert!(
            out.html.contains(r#"href="https://x.test/?e=alice"#),
            "{}",
            out.html
        );
    }

    #[test]
    fn no_tracked_url_carries_contact_data() {
        let stored = tracked_freeze(
            Authored::Html(
                "<p>Hi {{ first_name }}</p><a href=\"https://a.test/?n={{ first_name }}\">a</a>\
                 <a href=\"https://b.test/\">b</a><a href=\"mailto:{{ email }}\">m</a>",
            ),
            &defs(),
            &[],
        );
        let out = tracked_render(&stored, &defs(), &json!({}), Some(TRK));
        let tracked: Vec<&str> = attr_values(&out.html, "href")
            .into_iter()
            .chain(attr_values(&out.html, "src"))
            .filter(|u| u.contains("/o/") || u.contains("/l/"))
            .collect();
        // The pixel and b.test's link.
        assert_eq!(tracked.len(), 2, "{}", out.html);
        for u in tracked
            .into_iter()
            .chain(stored.links().iter().map(String::as_str))
        {
            let lower = u.to_ascii_lowercase();
            assert!(
                !lower.contains("alice") && !lower.contains("%40") && !lower.contains('@'),
                "{u}"
            );
        }
    }

    #[test]
    fn a_refreeze_keeps_every_earlier_link_at_its_index() {
        let prior = ["https://old.test/".to_owned(), "https://b.test/".to_owned()];
        let stored = tracked_freeze(
            Authored::Html("<a href=\"https://b.test/\">b</a><a href=\"https://c.test/\">c</a>"),
            &defs(),
            &prior,
        );
        assert_eq!(
            stored.links(),
            ["https://old.test/", "https://b.test/", "https://c.test/"]
        );
        let out = tracked_render(&stored, &defs(), &json!({}), Some(TRK));
        assert_eq!(
            &attr_values(&out.html, "href")[..2],
            [
                "https://mail.example.test/l/TRK/1",
                "https://mail.example.test/l/TRK/2"
            ]
        );
    }

    #[test]
    fn a_rendition_frozen_before_tracking_renders_untracked() {
        let stored = tracked_freeze(
            Authored::Html("<a href=\"https://a.test/\">a</a>"),
            &defs(),
            &[],
        );
        let mut column = serde_json::to_value(&stored).expect("serialises");
        let links = column.as_object_mut().expect("an object").remove("links");
        assert_eq!(links, Some(json!(["https://a.test/"])));
        let old = Stored::from_column(&column).expect("a rendition without links still reads");
        assert!(old.links().is_empty());
        let out = tracked_render(&old, &defs(), &json!({}), Some(TRK));
        assert!(!out.html.contains("/l/"), "{}", out.html);
        assert!(
            out.html.contains(r#"href="https://a.test/""#),
            "{}",
            out.html
        );
    }

    #[test]
    fn a_template_rendition_has_no_links_key() {
        let b = button_blocks();
        for authored in [
            Authored::Html("<a href=\"https://a.test/\">a</a>"),
            Authored::Blocks(&b),
        ] {
            let stored = compile(authored, &BrandKit::default(), &defs(), &BTreeSet::new())
                .expect("compiles");
            assert!(stored.links().is_empty());
            let column = serde_json::to_value(&stored).expect("serialises");
            assert!(column.get("links").is_none(), "{column}");
        }
    }
}
