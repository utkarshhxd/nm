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
//! **A write to `brand_kits` invalidates every stored rendition in that workspace at once**, and
//! nothing here regenerates them: refusing rather than recompiling is what keeps each handler to
//! one locking statement, and re-rendering at send is the defect above. The bulk recompile path
//! that follows from this belongs to M2c, which is where a send first depends on it —
//! `.scratch/email-platform/issues/49-a-brand-kit-edit-invalidates-every-stored-rendition-at-once.md`.

pub mod block;
pub mod lint;
pub mod mjml;
pub mod render;
pub mod routes;
pub mod tag;
pub mod text;

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
        })
    }

    /// Everything that must hold before a contact's copy may be made. Once per send, not once per
    /// contact: [`Ready`] is cheap to use and impossible to obtain without this.
    ///
    /// `fingerprint` is [`fingerprint`] of the authored document and the brand kit as they are
    /// NOW. A mismatch means the stored classification describes a document that is no longer the
    /// one this template would produce — a stale classification, which is a security bug and not a
    /// cache miss, so it is refused rather than used.
    pub fn ready<'a>(
        &'a self,
        fingerprint: &str,
        defs: &[Definition],
    ) -> Result<Ready<'a>, StoredError> {
        if self.fingerprint != fingerprint {
            return Err(StoredError::Stale);
        }
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
        Ok(Ready(self))
    }
}

impl Stored {
    /// The custom fields this document's merge tags name.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
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
    pub fn render(&self, contact: &Contact<'_>) -> Result<Rendered, StoredError> {
        let html = render::substitute(&self.0.html, &self.0.nodes, contact)
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
    let registry =
        tag::Registry::from_definitions(defs).allowing(keep_archived.iter().map(String::as_str));
    let (compiled, text) = match authored {
        Authored::Blocks(blocks) => (
            mjml::compile_blocks(blocks, brand, &registry)?,
            Some(text::compile_text(blocks, brand, &registry)?),
        ),
        // A raw template has no block tree, so there is no plain-text skeleton to compile: its
        // text part comes from the substituted HTML at send (see `Ready::render`).
        Authored::Mjml(source) => (mjml::compile_mjml(source, &registry)?, None),
        Authored::Html(source) => (mjml::compile_html(source, &registry)?, None),
    };
    Ok(Stored {
        marker_format_version: MARKER_FORMAT_VERSION,
        fingerprint: fingerprint(authored, brand),
        fields: field_kinds(&compiled.nodes, defs),
        nodes: compiled.nodes,
        html: compiled.html,
        text,
    })
}

/// The key that invalidates a stored rendition: the authored document, the brand kit, the marker
/// format and the versions of the two crates whose output is stored.
///
/// A brand-kit edit alone changes this, which is the point — the kit is emitted into the MJML the
/// classifier ran over, so a kit change makes the stored classification describe a document that
/// is no longer produced.
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
    // cannot change the rendered document, so it must not invalidate it either.
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
    let digest: [u8; 32] = Sha256::digest(&buf).into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The custom fields an AST names, and the kind each has now. A system field (`email`,
/// `first_name`, …) has no definition and is not recorded: nothing can change it.
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
            })
            .expect("renders");
        assert!(!out.text.contains(render::MARK_START), "{}", out.text);
        assert!(out.text.contains("teal"), "{}", out.text);
    }
}
