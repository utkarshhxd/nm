// SPDX-License-Identifier: AGPL-3.0-only

//! Block tree -> MJML -> HTML, at SAVE time.
//!
//! ```text
//! save:  blocks --to_mjml--> template source --tag::compile--> AST --render::marked--> marked MJML
//!        --render_mjml--> marked HTML --render::classify--> refuse, or store
//! send:  render::substitute(stored HTML, AST, contact)        <- mrml never runs again
//! ```
//!
//! Callers use [`compile_blocks`] (a `blocks` template) or [`compile_mjml`] (a raw `mjml` one),
//! which run the whole sequence; `to_mjml` and `render_mjml` are private so that the order cannot
//! be got wrong by calling the obvious function. `render_mjml` is the only `mrml` call and it is a
//! save-time call: its output is the document that is classified and stored, so it has to be
//! correct once, not cheap per contact. Do not add a per-contact render path here (see `render.rs`
//! for why that is the defect).
//!
//! `to_mjml` returns a template SOURCE, not yet MJML: a merge tag is left in as the operator
//! wrote it, because its fallback literal can hold the quotes an attribute value cannot.
//!
//! What `to_mjml` guarantees to the compile step:
//!
//! * **A tag may appear in a `Run::Tag` or in a URL field; everywhere else `{` is escaped.** Typed
//!   `{{ first_name }}` in a text run, link text, button label, image alt or the footer reaches the
//!   recipient as that text. The URL fields (`Link.href`, `Button.href`, `Image.src`) parse tags,
//!   because they have no atomic-token path; see [`emit`] for the reasoning. A raw `mjml` template
//!   has no `Run::Tag`, the operator wrote the source, so [`compile_mjml`] scans all of it.
//! * **It is deterministic.** Same blocks and brand kit, same bytes. Nothing here iterates a map
//!   or reads a clock; a re-save that reclassified a different document would leave every stored
//!   classification stale with nothing to notice.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::Value;

use super::block::{Block, Mark, Run};
use super::render::{self, MARK_START, RenderError};
use super::tag::{self, Node, Registry, TagError};

const DEFAULT_BACKGROUND: &str = "#ffffff";
const DEFAULT_TEXT: &str = "#111111";
const DEFAULT_ACCENT: &str = "#2563eb";
const DEFAULT_ACCENT_TEXT: &str = "#ffffff";
const DEFAULT_FONT: &str = "Arial, Helvetica, sans-serif";

/// What a workspace applies to every template (migration 0032, contract section 8).
///
/// Every field is optional because a workspace may have no kit, or a kit written before a key
/// existed. [`BrandKit::validate`] is the API gate; [`to_mjml`] does not trust that it ran and
/// drops any value that would not pass, so an unvalidated kit still cannot reach the output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrandKit {
    pub logo_url: Option<String>,
    pub background: Option<String>,
    pub text: Option<String>,
    pub accent: Option<String>,
    pub accent_text: Option<String>,
    pub font_stack: Option<String>,
    pub footer_text: Option<String>,
    pub footer_address: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BrandError {
    #[error("{field} must be a hex colour such as #fff or #1a2b3c")]
    Colour { field: &'static str },
    #[error(
        "font stack must be comma-separated font family names (letters, digits, spaces, hyphens \
         and underscores, optionally quoted) and nothing else"
    )]
    FontStack,
    #[error("logo must be an http or https URL")]
    Logo,
}

/// `mrml` hard-errors on input mjml.io accepts, so its own message is kept: it is the only thing
/// that tells an operator which element it disliked.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MjmlError {
    #[error("{0}")]
    Parse(String),
    #[error("{0}")]
    Render(String),
}

impl BrandKit {
    /// Build from a `brand_kits` row. Unknown keys are ignored here and are R7's to preserve in
    /// storage (contract section 8); a value of the wrong JSON type is dropped.
    pub fn from_row(
        logo_url: Option<&str>,
        palette: &Value,
        font_stack: Option<&str>,
        footer: &Value,
    ) -> Self {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Self {
            logo_url: logo_url.map(str::to_owned),
            background: s(palette, "background"),
            text: s(palette, "text"),
            accent: s(palette, "accent"),
            accent_text: s(palette, "accent_text"),
            font_stack: font_stack.map(str::to_owned),
            footer_text: s(footer, "text"),
            footer_address: s(footer, "address"),
        }
    }

    /// The one validator. R7 refuses an invalid kit by calling this, never a copy of it.
    pub fn validate(&self) -> Result<(), BrandError> {
        for (field, v) in [
            ("background", &self.background),
            ("text", &self.text),
            ("accent", &self.accent),
            ("accent_text", &self.accent_text),
        ] {
            if v.as_deref().is_some_and(|c| !is_colour(c)) {
                return Err(BrandError::Colour { field });
            }
        }
        if self
            .font_stack
            .as_deref()
            .is_some_and(|f| font_stack(f).is_none())
        {
            return Err(BrandError::FontStack);
        }
        if self.logo_url.as_deref().is_some_and(|l| logo(l).is_none()) {
            return Err(BrandError::Logo);
        }
        Ok(())
    }

    /// The colour if it is valid, else the default. Never the raw value.
    fn colour<'a>(v: &'a Option<String>, default: &'a str) -> &'a str {
        v.as_deref().filter(|c| is_colour(c)).unwrap_or(default)
    }
}

/// Frozen (contract section 8): `#rgb` or `#rrggbb`, case-insensitive.
fn is_colour(v: &str) -> bool {
    v.strip_prefix('#')
        .is_some_and(|h| matches!(h.len(), 3 | 6) && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A font stack in the one shape that cannot leave its CSS declaration: up to ten family names
/// separated by commas, each optionally quoted, made of letters, digits, spaces, `-` and `_`.
/// Returned canonicalised (every name that has a space is single-quoted) so the emitted text is
/// built from the parsed names, never copied from the input.
///
/// A grammar and not `escape_css`: the value is rendered into a `style` attribute by `mrml` and
/// into the hosted page's CSS, and a CSS escape (`\3b `) makes a font name that no longer matches
/// the font. `;`, `}`, `(`, `\`, `/` and a stray quote are not in the alphabet, so
/// `Arial; } body { background: url(...)` is not a font stack.
fn font_stack(raw: &str) -> Option<String> {
    let families: Vec<&str> = raw.split(',').map(str::trim).collect();
    if raw.len() > 200 || families.len() > 10 {
        return None;
    }
    let mut out = Vec::with_capacity(families.len());
    for f in families {
        let name = match f.chars().next() {
            Some(q @ ('\'' | '"')) => f.strip_prefix(q)?.strip_suffix(q)?,
            _ => f,
        };
        let ok = !name.is_empty()
            && name.trim() == name
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-'));
        if !ok {
            return None;
        }
        out.push(if name.contains(' ') {
            format!("'{name}'")
        } else {
            name.to_owned()
        });
    }
    Some(out.join(", "))
}

/// Contract section 11: `http` and `https` only, no userinfo. Returns the parser's normalised
/// form, which is what gets emitted: a brace in the input comes out as `%7B`.
fn logo(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw).ok()?;
    (matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none())
        .then(|| u.to_string())
}

// ---------------------------------------------------------------------------------------------
// Emitting

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Text,
    Attr,
}

/// Write `s` into the template source with merge tags let through: a tag verbatim, anything else
/// in the string escaped for `ctx`.
///
/// THE SPLIT, in one place. In a block template a tag may appear in a `Run::Tag`, or in a URL field
/// (`Link.href`, `Button.href`, `Image.src`, via [`url_attr`]). Everywhere else `{` is escaped by
/// [`literal`], so `{{ first_name }}` typed there reaches the recipient as that text.
///
/// Why those two and no others: in a text run the editor already offers an atomic `Run::Tag`, so
/// parsing the text too would be a second, invisible way to make a tag that bypasses the first's
/// guarantees, and help copy that MENTIONS a tag would silently substitute. A URL field has no such
/// token (the editor shows a plain input) so parsing is the only path and bypasses nothing, and
/// per-recipient links (unsubscribe, tracking, personalised CTAs) are the ordinary case. R3
/// already encodes a tag in a URL for the component it lands in and refuses one in the scheme or
/// host. A label, alt text or footer has no tag path; if one is ever wanted that is a contract
/// change, not a parse rule.
///
/// A Run::Tag's `source` goes through here too, so an editor bug or forged request that puts markup
/// next to the tag in `source` still produces none: whatever is not the tag is escaped text.
fn emit(s: &str, ctx: Ctx, out: &mut String) {
    let mut rest = s;
    // The compiler's rule, byte for byte: a `{{`, then the first `}}` after it.
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + len + 2;
        literal(&rest[..start], ctx, out);
        out.push_str(&rest[start..end]);
        rest = &rest[end..];
    }
    literal(rest, ctx, out);
}

/// Escape FIRST, then turn a newline into `<br>`. Each character is handled once, so the `<br>`
/// written for a newline is never itself escaped; the other order ships `&lt;br&gt;`.
///
/// `{` is escaped so that no literal can ever begin a `{{` that the compiler would pair with a
/// `}}` in a different field, which is also what keeps typed tag syntax inert.
fn literal(s: &str, ctx: Ctx, out: &mut String) {
    for c in s.chars() {
        match (c, ctx) {
            ('&', _) => out.push_str("&amp;"),
            ('<', _) => out.push_str("&lt;"),
            ('>', _) => out.push_str("&gt;"),
            ('{', _) => out.push_str("&#123;"),
            ('"', Ctx::Attr) => out.push_str("&quot;"),
            ('\'', Ctx::Attr) => out.push_str("&#39;"),
            ('\r', Ctx::Text) => {}
            ('\n', Ctx::Text) => out.push_str("<br>"),
            (c, Ctx::Attr) if c.is_control() => out.push(' '),
            (c, _) => out.push(c),
        }
    }
}

/// An attribute whose value is plain text: `{` is escaped.
fn attr(name: &str, value: &str, out: &mut String) {
    let _ = write!(out, " {name}=\"");
    literal(value, Ctx::Attr, out);
    out.push('"');
}

/// An attribute whose value is a URL: tags are live (see [`emit`]).
fn url_attr(name: &str, value: &str, out: &mut String) {
    let _ = write!(out, " {name}=\"");
    emit(value, Ctx::Attr, out);
    out.push('"');
}

fn marked(marks: &[Mark], out: &mut String, inner: impl FnOnce(&mut String)) {
    let (bold, italic) = (marks.contains(&Mark::Bold), marks.contains(&Mark::Italic));
    if bold {
        out.push_str("<strong>");
    }
    if italic {
        out.push_str("<em>");
    }
    inner(out);
    if italic {
        out.push_str("</em>");
    }
    if bold {
        out.push_str("</strong>");
    }
}

fn runs(rs: &[Run], accent: &str, out: &mut String) {
    for r in rs {
        match r {
            Run::Text { text, marks } => marked(marks, out, |o| literal(text, Ctx::Text, o)),
            Run::Link { href, text, marks } => {
                out.push_str("<a");
                url_attr("href", href, out);
                let _ = write!(out, " style=\"color:{accent}\">");
                marked(marks, out, |o| literal(text, Ctx::Text, o));
                out.push_str("</a>");
            }
            Run::Tag { source } => emit(source, Ctx::Text, out),
        }
    }
}

/// One block inside a column. `Columns` has no place here (MJML cannot render a section inside a
/// column) and deserialisation refuses it, so a hand-built one is dropped rather than recursed.
fn leaf(b: &Block, accent: &str, out: &mut String) {
    match b {
        Block::Text { runs: rs } => {
            out.push_str("<mj-text>");
            runs(rs, accent, out);
            out.push_str("</mj-text>");
        }
        Block::Heading { level, runs: rs } => {
            let l = (*level).clamp(1, 6);
            let _ = write!(out, "<mj-text><h{l} style=\"margin:0\">");
            runs(rs, accent, out);
            let _ = write!(out, "</h{l}></mj-text>");
        }
        Block::Image { src, alt } => {
            out.push_str("<mj-image");
            url_attr("src", src, out);
            attr("alt", alt, out);
            out.push_str(" />");
        }
        Block::Button { label, href } => {
            out.push_str("<mj-button");
            url_attr("href", href, out);
            out.push('>');
            literal(label, Ctx::Text, out);
            out.push_str("</mj-button>");
        }
        Block::Divider {} => out.push_str("<mj-divider />"),
        Block::Spacer { height } => {
            let _ = write!(out, "<mj-spacer height=\"{height}px\" />");
        }
        Block::Columns { .. } => {}
    }
}

/// A one-column section around a single piece of body content.
fn section(inner: impl FnOnce(&mut String), out: &mut String) {
    out.push_str("<mj-section><mj-column>");
    inner(out);
    out.push_str("</mj-column></mj-section>\n");
}

/// The template source for `blocks` under `brand`. See the module doc.
fn to_mjml(blocks: &[Block], brand: &BrandKit) -> String {
    let accent = BrandKit::colour(&brand.accent, DEFAULT_ACCENT);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<mjml><mj-head><mj-attributes><mj-all font-family=\"{}\" />\
         <mj-text color=\"{}\" /><mj-button background-color=\"{accent}\" color=\"{}\" />\
         </mj-attributes></mj-head><mj-body background-color=\"{}\">",
        brand
            .font_stack
            .as_deref()
            .and_then(font_stack)
            .unwrap_or_else(|| DEFAULT_FONT.to_owned()),
        BrandKit::colour(&brand.text, DEFAULT_TEXT),
        BrandKit::colour(&brand.accent_text, DEFAULT_ACCENT_TEXT),
        BrandKit::colour(&brand.background, DEFAULT_BACKGROUND),
    );

    if let Some(url) = brand.logo_url.as_deref().and_then(logo) {
        section(
            |o| {
                // The URL parser encodes a brace in a path but NOT in a query or fragment, so
                // `literal` is what keeps `?{{first_name}}` from compiling to a live tag.
                o.push_str("<mj-image width=\"150px\" alt=\"\" src=\"");
                literal(&url, Ctx::Attr, o);
                o.push_str("\" />");
            },
            &mut out,
        );
    }

    for b in blocks {
        match b {
            Block::Columns { children } if !children.is_empty() => {
                out.push_str("<mj-section>");
                for col in children {
                    out.push_str("<mj-column>");
                    for child in col {
                        leaf(child, accent, &mut out);
                    }
                    out.push_str("</mj-column>");
                }
                out.push_str("</mj-section>\n");
            }
            Block::Columns { .. } => {}
            other => section(|o| leaf(other, accent, o), &mut out),
        }
    }

    let footer: Vec<&str> = [&brand.footer_text, &brand.footer_address]
        .into_iter()
        .filter_map(|f| f.as_deref())
        .filter(|f| !f.trim().is_empty())
        .collect();
    if !footer.is_empty() {
        section(
            |o| {
                o.push_str("<mj-text align=\"center\" font-size=\"12px\">");
                for (i, f) in footer.iter().enumerate() {
                    if i > 0 {
                        o.push_str("<br>");
                    }
                    literal(f, Ctx::Text, o);
                }
                o.push_str("</mj-text>");
            },
            &mut out,
        );
    }

    out.push_str("</mj-body></mjml>\n");
    out
}

/// A template that has been through the whole save pipeline and is safe to store.
///
/// `html` is `mrml`'s output with the merge-tag markers still in it, already classified: this is
/// the document every send substitutes into. `nodes` is the AST it was marked from. Store both
/// (and `render::MARKER_FORMAT_VERSION` beside them) and pass them to `render::substitute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub nodes: Vec<Node>,
    pub html: String,
}

/// Why a template could not be saved. Each arm is the stage that refused it, and `Display` reads
/// as a sentence an operator can act on (R7 surfaces these).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    #[error("the brand kit is invalid: {0}")]
    Brand(#[from] BrandError),
    #[error("{}", tag_message(.0))]
    Tag(TagError),
    #[error(
        "the template contains the text {MARK_START}, which is reserved for the renderer's own use; remove it"
    )]
    ForgedMarker,
    #[error("the template could not be rendered as MJML: {0}")]
    Mjml(#[from] MjmlError),
    /// [`RenderError::DangerousContext`] with its position translated back into the operator's own
    /// text. The raw position is a byte offset into **`mrml`'s output**, a document the operator
    /// has never seen, so a refusal carrying only that number is correct and unactionable — which
    /// for the operator is the same as a bug. This is the only place the classified document and
    /// the template source are both in hand, so the mapping is made here.
    #[error(
        "the merge tag {tag} is in a position where no encoding can make it safe: inside a script \
         or comment, in a tag or attribute name, or in the scheme or host of a link"
    )]
    DangerousTag { tag: String },
    #[error("{}", render_message(.0))]
    Render(RenderError),
}

/// The `index`-th merge tag in `source`, exactly as the operator wrote it.
///
/// [`render::marked`] numbers slots in the order [`tag::compile`] produced them, which is the
/// order the tags appear in the source. A block's CLOSING `{{/ key }}` takes no slot of its own,
/// so it is skipped here too. The tag scanning rule is `tag::compile`'s, byte for byte: a `{{`,
/// then the first `}}` after it.
fn nth_tag(source: &str, index: usize) -> Option<&str> {
    let (mut seen, mut at) = (0usize, 0usize);
    while let Some(offset) = source[at..].find("{{") {
        let start = at + offset;
        let len = source[start + 2..].find("}}")?;
        let end = start + 2 + len + 2;
        at = end;
        if source[start + 2..end].trim_start().starts_with('/') {
            continue;
        }
        if seen == index {
            return Some(&source[start..end]);
        }
        seen += 1;
    }
    None
}

/// The slot index of the marker that starts at `position` in the classified document.
fn marker_index(html: &str, position: usize) -> Option<usize> {
    let body = html.get(position..)?.strip_prefix(render::MARK_START)?;
    let digits = &body[..body.find(render::MARK_END)?];
    // A conditional's boundary marker carries an `o` or `c` after its index.
    digits
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .ok()
}

fn tag_message(e: &TagError) -> String {
    match e {
        TagError::UnknownField(k) => {
            format!("a merge tag names the field \"{k}\", which does not exist")
        }
        TagError::ArchivedField(k) => {
            format!("a merge tag names the field \"{k}\", which has been archived")
        }
        TagError::UnknownFilter(f) => format!(
            "a merge tag uses the filter \"{f}\"; the filters are upper, lower, title and date"
        ),
        TagError::FilterAndFallback => {
            "a merge tag has both a filter and a fallback; use one or the other".into()
        }
        TagError::NonLiteralFallback => {
            "a merge tag's fallback must be a quoted piece of text".into()
        }
        TagError::Unbalanced(k) => {
            format!("the merge-tag block for \"{k}\" is not closed, or is closed out of order")
        }
        TagError::TooDeep => "merge-tag blocks are nested too deeply".into(),
        TagError::Malformed(why) => format!("a merge tag is malformed: {why}"),
    }
}

pub(crate) fn render_message(e: &RenderError) -> String {
    match e {
        RenderError::DangerousContext { .. } => "a merge tag is in a position where no encoding can make it safe: inside a script or comment, in a tag or attribute name, or in the scheme or host of a link".into(),
        RenderError::TagLost { .. } => "a merge tag was dropped while the template was rendered, so recipients would not see it; it is probably in an attribute the email renderer ignores".into(),
        RenderError::BlockDuplicated { .. } => "a conditional merge-tag block appears more than once in the rendered email, so which part to hide is ambiguous".into(),
        RenderError::BlockStraddlesTag { .. } => "a conditional merge-tag block starts or ends in the middle of an HTML tag; it must wrap whole elements or whole text".into(),
        RenderError::BlockUnbalanced { .. } => "a conditional merge-tag block is out of order or not properly nested in the rendered email".into(),
    }
}

/// Save a `blocks` template: the whole pipeline in one call, so its order is not something a
/// caller can get wrong. In a block template a merge tag comes from a `Run::Tag` and from nowhere
/// else but a URL field (`Link.href`, `Button.href`, `Image.src`): `{{ x }}` typed into a text run,
/// a label, alt text or the footer is ordinary text.
pub fn compile_blocks(
    blocks: &[Block],
    brand: &BrandKit,
    registry: &Registry,
) -> Result<Compiled, CompileError> {
    // An invalid kit is an error, never a different email than the one configured: `to_mjml` would
    // drop a bad logo and swap a bad font for the default without a word.
    brand.validate()?;
    compile(&to_mjml(blocks, brand), registry, true)
}

/// Save a raw `mjml` template. Unlike a block template the operator writes the source directly and
/// there is no `Run::Tag`, so the WHOLE source is scanned for merge tags.
pub fn compile_mjml(source: &str, registry: &Registry) -> Result<Compiled, CompileError> {
    compile(source, registry, true)
}

/// Save a raw `html` template: the same pipeline with the `mrml` step removed, because there is no
/// MJML to render and running one over hand-written HTML would rewrite the operator's document.
///
/// The SHAPE is deliberately identical to the other two — mark the tags, classify once, store the
/// marked document, substitute per contact — so a raw HTML template is not a second rendering
/// model with its own escaping rules. `Compiled::html` here is the marked source itself.
pub fn compile_html(source: &str, registry: &Registry) -> Result<Compiled, CompileError> {
    compile(source, registry, false)
}

fn has_literal_marker(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Text(t) => t.contains(MARK_START),
        Node::Field { .. } => false,
        Node::Present { body, .. } | Node::Absent { body, .. } | Node::Equals { body, .. } => {
            has_literal_marker(body)
        }
    })
}

/// compile the tags -> mark -> `mrml` once (when `mjml`) -> classify. `source` is template source,
/// not yet MJML: a tag's fallback literal can hold quotes that an attribute cannot.
fn compile(source: &str, registry: &Registry, mjml: bool) -> Result<Compiled, CompileError> {
    let nodes = tag::compile(source, registry).map_err(CompileError::Tag)?;
    // Contract section 13: there is exactly one way to make a tag. A literal marker in template
    // text would alias to a real tag of the same index (a send printed a contact's name where the
    // operator typed text), which is the shortcut `render::MARK_START` documents and leaves to
    // this compile path. Refused, not escaped: an escape leans on how mrml and the HTML parser
    // round-trip a character reference, and no real template contains the text. A tag's fallback
    // is data in the AST, never emitted as MJML, so only `Node::Text` is checked.
    if has_literal_marker(&nodes) {
        return Err(CompileError::ForgedMarker);
    }
    let (marked, count) = render::marked(&nodes);
    let html = if mjml { render_mjml(&marked)? } else { marked };
    if let Err(e) = render::classify(&html, count) {
        // The one refusal whose position means nothing to the operator. Falls back to the plain
        // message when the offset is not a marker start, rather than guessing at a tag.
        if let RenderError::DangerousContext { position } = &e
            && let Some(tag) = marker_index(&html, *position).and_then(|i| nth_tag(source, i))
        {
            return Err(CompileError::DangerousTag {
                tag: tag.to_owned(),
            });
        }
        return Err(CompileError::Render(e));
    }
    Ok(Compiled { nodes, html })
}

/// Render marked MJML to HTML. The only `mrml` call in the product, and a save-time one.
///
/// `fonts` is emptied: the default set would add `<link>` tags to Google's servers for any font
/// stack that names one, which is a third-party request per open, and a `HashMap` is the one place
/// iteration order could leak into the output.
fn render_mjml(mjml: &str) -> Result<String, MjmlError> {
    let parsed = mrml::parse(mjml).map_err(|e| MjmlError::Parse(e.to_string()))?;
    let opts = mrml::prelude::render::RenderOptions {
        fonts: HashMap::new(),
        ..Default::default()
    };
    parsed
        .element
        .render(&opts)
        .map_err(|e| MjmlError::Render(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::block::{Mark, Run};
    use crate::template::render::Contact;
    use serde_json::json;

    fn text(s: &str) -> Run {
        Run::Text {
            text: s.into(),
            marks: vec![],
        }
    }
    fn para(s: &str) -> Block {
        Block::Text {
            runs: vec![text(s)],
        }
    }
    fn tag(source: &str) -> Run {
        Run::Tag {
            source: source.into(),
        }
    }
    fn button(href: &str) -> Block {
        Block::Button {
            label: "b".into(),
            href: href.into(),
        }
    }

    /// Every block variant once, columns last.
    fn everything() -> Vec<Block> {
        vec![
            para("Hello"),
            Block::Heading {
                level: 3,
                runs: vec![text("Title")],
            },
            Block::Image {
                src: "https://img.example/a.png".into(),
                alt: "An image".into(),
            },
            Block::Button {
                label: "Press me".into(),
                href: "https://btn.example/go".into(),
            },
            Block::Divider {},
            Block::Spacer { height: 17 },
            Block::Columns {
                children: vec![
                    vec![para("left one"), Block::Divider {}],
                    vec![para("right one")],
                ],
            },
        ]
    }

    type Saved = Compiled;

    fn save(blocks: &[Block], brand: &BrandKit) -> Result<Saved, String> {
        compile_blocks(blocks, brand, &Registry::from_definitions(&[])).map_err(|e| e.to_string())
    }

    fn send(s: &Saved, first_name: &str) -> String {
        let attrs = json!({});
        let c = Contact {
            email: "a@b.example",
            first_name,
            last_name: "",
            timezone: None,
            attributes: &attrs,
        };
        render::substitute(&s.html, &s.nodes, &c).unwrap()
    }

    fn html_of(blocks: &[Block]) -> String {
        save(blocks, &BrandKit::default()).unwrap().html
    }

    #[test]
    fn every_block_variant_emits_mjml_that_mrml_accepts() {
        let html = html_of(&everything());
        for needle in [
            "Hello",
            "<h3",
            "Title",
            "https://img.example/a.png",
            "An image",
            "Press me",
            "https://btn.example/go",
            "border-top",  // mj-divider
            "height:17px", // mj-spacer
            "left one",
            "right one",
        ] {
            assert!(html.contains(needle), "{needle} missing from:\n{html}");
        }
    }

    #[test]
    fn columns_keep_their_children_in_their_own_column() {
        let html = html_of(&everything());
        let (l, r) = (
            html.find("left one").unwrap(),
            html.find("right one").unwrap(),
        );
        assert!(l < r);
        // A real mrml column each, not one flattened stack.
        assert!(html.matches("mj-column-per-50").count() >= 2, "{html}");
    }

    #[test]
    fn the_brand_kits_colours_and_font_reach_the_output() {
        let brand = BrandKit {
            background: Some("#fafafa".into()),
            text: Some("#123456".into()),
            accent: Some("#abc".into()),
            accent_text: Some("#FEDCBA".into()),
            font_stack: Some("'Helvetica Neue', Arial, sans-serif".into()),
            ..BrandKit::default()
        };
        let blocks = vec![para("x"), button("https://a.example/")];
        let html = save(&blocks, &brand).unwrap().html;
        for needle in ["#fafafa", "#123456", "#abc", "#FEDCBA", "Helvetica Neue"] {
            assert!(html.contains(needle), "{needle} missing from:\n{html}");
        }
    }

    #[test]
    fn the_logo_and_footer_are_emitted() {
        let brand = BrandKit {
            logo_url: Some("https://cdn.example/logo.png".into()),
            footer_text: Some("You subscribed.".into()),
            footer_address: Some("1 Main St\nTown".into()),
            ..BrandKit::default()
        };
        let html = save(&[para("x")], &brand).unwrap().html;
        assert!(html.contains("https://cdn.example/logo.png"));
        assert!(html.contains("You subscribed."));
        assert!(html.contains("1 Main St<br />Town"), "{html}");
    }

    #[test]
    fn an_mrml_parse_failure_carries_mrmls_own_message() {
        let bad = "<mjml><mj-body><mj-text>unclosed</mj-body></mjml>";
        let theirs = mrml::parse(bad)
            .err()
            .expect("mrml must reject this")
            .to_string();
        let ours = render_mjml(bad).unwrap_err();
        assert!(ours.to_string().contains(&theirs), "{ours} vs {theirs}");
        assert!(matches!(ours, MjmlError::Parse(_)));
    }

    /// Holds for ONE version of `mrml` and `url`: both feed the bytes this compares, and a bump of
    /// either can change them. `MARKER_FORMAT_VERSION` does not cover a dependency bump, so after
    /// one a re-save reclassifies different bytes than the stored copy was made from. Whoever
    /// bumps either crate must treat stored marked HTML as stale (R7's invalidation key).
    #[test]
    fn the_same_input_gives_byte_identical_output() {
        let brand = BrandKit {
            background: Some("#fafafa".into()),
            font_stack: Some("Arial, sans-serif".into()),
            logo_url: Some("https://cdn.example/l.png".into()),
            footer_text: Some("f".into()),
            ..BrandKit::default()
        };
        let blocks = everything();
        let (a, b) = (to_mjml(&blocks, &brand), to_mjml(&blocks, &brand));
        assert_eq!(a, b);
        let nodes = tag::compile(&a, &Registry::from_definitions(&[])).unwrap();
        let marked = render::marked(&nodes).0;
        // Rendering is part of what must repeat: a HashMap inside mrml would show up here.
        let first = render_mjml(&marked).unwrap();
        for _ in 0..8 {
            assert_eq!(render_mjml(&marked).unwrap(), first);
        }
    }

    // ----- the brand kit ----------------------------------------------------------------------

    /// What `compile_blocks` says about a kit, with an otherwise fine template.
    fn kit_result(brand: &BrandKit) -> Result<Compiled, CompileError> {
        compile_blocks(&[para("x")], brand, &Registry::from_definitions(&[]))
    }

    fn with_font(f: &str) -> BrandKit {
        BrandKit {
            font_stack: Some(f.into()),
            ..BrandKit::default()
        }
    }

    // ----- font_stack: a CSS injection site ---------------------------------------------------

    #[test]
    fn a_font_stack_that_breaks_out_of_its_declaration_is_refused_and_never_emitted() {
        for hostile in [
            "Arial; } body { background: url(https://evil.example/x) ",
            "Arial;color:red",
            "Arial } body",
            "Arial, url(https://evil.example/x)",
            "Arial\"><script>x</script>",
            "Arial\\",
            "Arial /* x */",
            "",
            "   ",
            ",",
            "Arial,,sans-serif",
        ] {
            let brand = with_font(hostile);
            assert!(brand.validate().is_err(), "{hostile:?} was accepted");
            // compile_blocks refuses: a bad kit is an error, not a different email.
            assert!(
                matches!(
                    kit_result(&brand),
                    Err(CompileError::Brand(BrandError::FontStack))
                ),
                "{hostile:?}"
            );
            // And to_mjml holds the line on its own, in case a caller ever skips the gate.
            let out = to_mjml(&[para("x")], &brand);
            for bad in ["evil", "url(", "script", "} body", "color:red"] {
                assert!(!out.contains(bad), "{hostile:?} leaked {bad:?}:\n{out}");
            }
        }
    }

    /// Every single character outside the alphabet is refused ON ITS OWN. The hostile values above
    /// each carry several forbidden characters, so on their own they would still be refused if any
    /// one of them were quietly allowed. `"` alone is an attribute breakout in the emitted
    /// `font-family="..."`.
    #[test]
    fn the_font_stack_alphabet_is_pinned_one_character_at_a_time() {
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | ',');
        for c in (0u8..128).map(char::from).filter(|c| !allowed(*c)) {
            for stack in [format!("a{c}b"), format!("Arial, a{c}b, serif")] {
                assert!(
                    with_font(&stack).validate().is_err(),
                    "{c:?} ({stack:?}) was accepted"
                );
            }
        }
        // A quote is only a wrapper, never a character of a name.
        for stack in [
            "'a\"b'",
            "\"a'b\"",
            "'a'b'",
            "'unclosed",
            "\"mismatched'",
            "''",
            "' a'",
        ] {
            assert!(with_font(stack).validate().is_err(), "{stack:?}");
        }
        // Letters and parentheses only is still not a font.
        assert!(with_font("expression(x)").validate().is_err());
        assert!(with_font("expression(alert(1))").validate().is_err());
        // The allowed set, including what looks like it might not be.
        for ok in ["a b", "a_b", "a-b", "Arial,Georgia", "a1"] {
            with_font(ok)
                .validate()
                .unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
    }

    #[test]
    fn the_font_stack_length_caps_are_pinned() {
        let long = |n: usize| "a".repeat(n);
        with_font(&long(200)).validate().unwrap();
        assert!(with_font(&long(201)).validate().is_err());
        let families = |n: usize| vec!["a"; n].join(",");
        with_font(&families(10)).validate().unwrap();
        assert!(with_font(&families(11)).validate().is_err());
    }

    #[test]
    fn a_plain_font_stack_is_accepted_and_emitted() {
        for ok in [
            "Arial, Helvetica, sans-serif",
            "'Helvetica Neue', Arial, sans-serif",
            "\"Segoe UI\", Roboto",
            "Georgia",
            "Noto Sans JP, sans-serif",
        ] {
            let brand = with_font(ok);
            brand.validate().unwrap_or_else(|e| panic!("{ok:?}: {e}"));
            let html = kit_result(&brand).unwrap().html;
            let first = ok
                .split(',')
                .next()
                .unwrap()
                .trim()
                .trim_matches(['\'', '"']);
            assert!(html.contains(first), "{first} missing:\n{html}");
        }
    }

    #[test]
    fn a_colour_must_be_rgb_or_rrggbb_and_nothing_wider() {
        // Contract section 8 freezes the format; the client enforces exactly these two, and a
        // server that accepts #rgba or #rrggbbaa is how the two lanes drift apart.
        for bad in [
            "red",
            "#12",
            "#abcd",
            "#12345",
            "#1234567",
            "#aabbccdd",
            "#ggg",
            "#",
            "fff",
            "url(x)",
            "#fff; x",
            "#fff ",
            "rgb(0,0,0)",
        ] {
            let brand = BrandKit {
                accent: Some(bad.into()),
                ..BrandKit::default()
            };
            assert!(brand.validate().is_err(), "{bad}");
            assert!(
                matches!(
                    kit_result(&brand),
                    Err(CompileError::Brand(BrandError::Colour { field: "accent" }))
                ),
                "{bad}"
            );
            let out = to_mjml(&[para("x"), button("https://a.example/")], &brand);
            // (Skipped for values that are substrings of the default colours.)
            if !matches!(bad, "#" | "fff") {
                assert!(!out.contains(bad), "{bad} leaked");
            }
        }
        for ok in ["#fff", "#FFF", "#a1B2c3"] {
            BrandKit {
                accent: Some(ok.into()),
                ..BrandKit::default()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn every_colour_field_is_validated_and_named() {
        for field in ["background", "text", "accent", "accent_text"] {
            let mut brand = BrandKit::default();
            let slot = match field {
                "background" => &mut brand.background,
                "text" => &mut brand.text,
                "accent" => &mut brand.accent,
                _ => &mut brand.accent_text,
            };
            *slot = Some("nope".into());
            let err = brand.validate().unwrap_err();
            assert!(err.to_string().starts_with(field), "{field}: {err}");
        }
    }

    #[test]
    fn a_logo_must_be_http_or_https_without_userinfo() {
        for bad in [
            "mailto:a@b.example",
            "tel:+1",
            "javascript:alert(1)",
            "data:image/png;base64,AAAA",
            "//cdn.example/x.png",
            "not a url",
            "https://user:pw@cdn.example/x.png",
            // The two userinfo checks are separate: either alone is enough to refuse.
            "https://user@cdn.example/x.png",
            "https://:pw@cdn.example/x.png",
        ] {
            let brand = BrandKit {
                logo_url: Some(bad.into()),
                ..BrandKit::default()
            };
            assert!(brand.validate().is_err(), "{bad}");
            assert!(
                matches!(
                    kit_result(&brand),
                    Err(CompileError::Brand(BrandError::Logo))
                ),
                "{bad}"
            );
            let out = to_mjml(&[para("x")], &brand);
            assert!(
                !out.contains("javascript") && !out.contains("mailto") && !out.contains("data:"),
                "{out}"
            );
        }
        for ok in ["http://a.example/l.png", "https://a.example/l.png?x=1&y=2"] {
            BrandKit {
                logo_url: Some(ok.into()),
                ..BrandKit::default()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn a_logo_url_cannot_become_a_tag_through_its_query_or_fragment() {
        // The `url` crate leaves `{` alone in a query and a fragment (it encodes it in a path), so
        // the escaping in `literal` is what keeps a logo URL from compiling to a live tag.
        let brand = BrandKit {
            logo_url: Some("https://a.example/l.png?{{first_name}}#{{email}}".into()),
            ..BrandKit::default()
        };
        brand.validate().unwrap();
        let src = to_mjml(&[], &brand);
        assert!(src.contains("https://a.example/l.png"), "{src}");
        let nodes = tag::compile(&src, &Registry::from_definitions(&[])).unwrap();
        assert!(matches!(nodes.as_slice(), [Node::Text(_)]), "{nodes:?}");
        // And end to end: nothing for a contact to be substituted into.
        let saved = kit_result(&brand).unwrap();
        assert!(!send(&saved, "Ann").contains("Ann"));
    }

    #[test]
    fn an_invalid_kit_is_an_error_not_a_different_email() {
        // Previously a bad logo was dropped and a bad font replaced with the default, silently.
        let brand = BrandKit {
            logo_url: Some("javascript:alert(1)".into()),
            ..BrandKit::default()
        };
        let err = kit_result(&brand).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the brand kit is invalid: logo must be an http or https URL"
        );
    }

    #[test]
    fn the_footer_is_text_then_address() {
        let brand = BrandKit {
            footer_text: Some("FOOTTEXT".into()),
            footer_address: Some("FOOTADDRESS".into()),
            ..BrandKit::default()
        };
        let html = kit_result(&brand).unwrap().html;
        let (t, a) = (
            html.find("FOOTTEXT").unwrap(),
            html.find("FOOTADDRESS").unwrap(),
        );
        assert!(t < a, "{html}");
        // Either one alone; blank is the same as absent, so no empty footer block.
        let only = BrandKit {
            footer_address: Some("ONLYADDR".into()),
            footer_text: Some("  ".into()),
            ..BrandKit::default()
        };
        let html = kit_result(&only).unwrap().html;
        assert!(html.contains("ONLYADDR"), "{html}");
        // mrml may tidy the HTML, so the blank part is pinned in the source it is given.
        let src = to_mjml(&[], &only);
        assert!(
            src.contains("font-size=\"12px\">ONLYADDR</mj-text>"),
            "{src}"
        );
    }

    #[test]
    fn from_row_reads_the_contracts_keys_and_ignores_unknown_ones() {
        let brand = BrandKit::from_row(
            Some("https://a.example/l.png"),
            &json!({"background":"#fff","text":"#000","accent":"#111","accent_text":"#222","future":"x"}),
            Some("Arial"),
            &json!({"text":"t","address":"a","future":1}),
        );
        assert_eq!(brand.accent.as_deref(), Some("#111"));
        assert_eq!(brand.accent_text.as_deref(), Some("#222"));
        assert_eq!(brand.footer_address.as_deref(), Some("a"));
        assert_eq!(brand.footer_text.as_deref(), Some("t"));
        assert_eq!(brand.logo_url.as_deref(), Some("https://a.example/l.png"));
        // A non-string where a string belongs is dropped, not a panic.
        let odd = BrandKit::from_row(None, &json!({"accent": 7}), None, &json!({"text": []}));
        assert_eq!(odd, BrandKit::default());
    }

    // ----- escaping and line breaks -----------------------------------------------------------

    #[test]
    fn run_text_is_escaped_for_html() {
        let html = html_of(&[para("<script>alert(1)</script> & <b>")]);
        assert!(!html.contains("<script>alert"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; &lt;b&gt;"),
            "{html}"
        );
    }

    #[test]
    fn a_newline_becomes_br_after_escaping_not_before() {
        let html = html_of(&[para("a<b\nc & d\r\ne")]);
        assert!(html.contains("a&lt;b<br />c &amp; d<br />e"), "{html}");
        // The wrong order escapes the break itself and ships it to the recipient as text.
        assert!(!html.contains("&lt;br&gt;"), "{html}");
    }

    #[test]
    fn a_link_runs_text_breaks_too() {
        let blocks = [Block::Text {
            runs: vec![Run::Link {
                href: "https://a.example/".into(),
                text: "one\ntwo".into(),
                marks: vec![],
            }],
        }];
        let html = html_of(&blocks);
        assert!(html.contains("one<br />two"), "{html}");
        assert!(!html.contains("&lt;br&gt;"));
    }

    #[test]
    fn a_substituted_values_newlines_are_never_converted() {
        let blocks = [Block::Text {
            runs: vec![text("Hi "), tag("{{ first_name }}"), text("!\nBye")],
        }];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        let out = send(&saved, "Al\nice <br> <i>");
        // The operator's own break is a <br>; the contact's is a newline and its markup is text.
        assert!(
            out.contains("Al\nice &lt;br&gt; &lt;i&gt;!<br />Bye"),
            "{out}"
        );
    }

    #[test]
    fn marks_nest_and_combine() {
        let blocks = [Block::Text {
            runs: vec![
                Run::Text {
                    text: "both".into(),
                    marks: vec![Mark::Bold, Mark::Italic],
                },
                Run::Text {
                    text: "dup".into(),
                    marks: vec![Mark::Bold, Mark::Bold],
                },
                Run::Link {
                    href: "https://a.example/".into(),
                    text: "ln".into(),
                    marks: vec![Mark::Italic],
                },
            ],
        }];
        let html = html_of(&blocks);
        let both = html.find("both").unwrap();
        let around = &html[both - 20..both + 20];
        assert!(
            around.contains("<strong>") && around.contains("<em>"),
            "{around}"
        );
        assert_eq!(html.matches("<strong>dup</strong>").count(), 1, "{html}");
        assert!(html.contains("<em>ln</em>"), "{html}");
        assert!(html.contains("href=\"https://a.example/\""), "{html}");
    }

    #[test]
    fn heading_levels_map_to_h1_through_h6() {
        for level in 1..=6u8 {
            let html = html_of(&[Block::Heading {
                level,
                runs: vec![text("T")],
            }]);
            assert!(
                html.contains(&format!("<h{level}")) && html.contains(&format!("</h{level}>")),
                "{html}"
            );
        }
    }

    // ----- merge tags -------------------------------------------------------------------------

    #[test]
    fn a_tag_run_becomes_a_live_tag_with_its_fallback_and_is_escaped_on_the_way_out() {
        let blocks = [Block::Text {
            runs: vec![text("Hi "), tag("{{ first_name | \"there\" }}")],
        }];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        assert!(send(&saved, "").contains("Hi there"));
        assert!(send(&saved, "Ann").contains("Hi Ann"));
        assert!(
            send(&saved, "<img src=x onerror=alert(1)>")
                .contains("Hi &lt;img src=x onerror=alert(1)&gt;")
        );
    }

    #[test]
    fn a_tag_typed_into_a_text_run_is_literal_text_not_a_tag() {
        // An operator writing help copy that MENTIONS a merge tag gets the text they typed, not
        // the recipient's name. A tag comes from Run::Tag and nowhere else.
        let blocks = [Block::Text {
            runs: vec![
                text(
                    "Use {{ first_name }} in your subject, or {{# first_name }}x{{/ first_name }}.",
                ),
                Run::Link {
                    href: "https://a.example/".into(),
                    text: "link {{ first_name }}".into(),
                    marks: vec![],
                },
            ],
        }];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        assert!(
            saved.nodes.iter().all(|n| matches!(n, Node::Text(_))),
            "{:?}",
            saved.nodes
        );
        let out = send(&saved, "Ann");
        assert!(
            out.contains(
                "Use {{ first_name }} in your subject, or {{# first_name }}x{{/ first_name }}."
            ),
            "{out}"
        );
        assert!(out.contains("link {{ first_name }}"), "{out}");
        assert!(!out.contains("Ann"), "{out}");
    }

    #[test]
    fn a_tag_in_a_url_field_is_live_and_encoded_for_its_component() {
        // Link.href, Button.href and Image.src are the one place besides Run::Tag that parses
        // tags: the editor shows a URL input there, so there is no atomic token to bypass.
        let blocks = [
            Block::Text {
                runs: vec![Run::Link {
                    href: "https://app.example.com/u/{{ first_name }}?e={{ email }}".into(),
                    text: "go".into(),
                    marks: vec![],
                }],
            },
            Block::Button {
                label: "b".into(),
                href: "https://app.example.com/b?e={{ email }}&n={{ first_name }}".into(),
            },
            Block::Image {
                src: "https://img.example.com/{{ first_name }}.png".into(),
                alt: "a".into(),
            },
        ];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        let out = send(&saved, "a&b/c d");
        // email is a@b.example (see `send`): `@` is legal in a query.
        assert!(
            out.contains("app.example.com/u/a&amp;b%2Fc%20d?e=a@b.example"),
            "{out}"
        );
        assert!(
            out.contains("app.example.com/b?e=a@b.example&amp;n=a%26b/c%20d"),
            "{out}"
        );
        assert!(out.contains("img.example.com/a&amp;b%2Fc%20d.png"), "{out}");
    }

    #[test]
    fn a_tag_in_the_scheme_or_host_of_a_url_field_is_still_refused() {
        let reg = Registry::from_definitions(&[]);
        for href in [
            "https://{{ first_name }}.example.com/",
            "https://app.example.com@{{ first_name }}/",
            "{{ first_name }}://app.example.com/",
        ] {
            let b = [Block::Button {
                label: "b".into(),
                href: href.into(),
            }];
            // The refusal names the operator's own tag, not a byte offset into `mrml` output.
            assert!(
                matches!(
                    compile_blocks(&b, &BrandKit::default(), &reg),
                    Err(CompileError::DangerousTag { ref tag }) if tag == "{{ first_name }}"
                ),
                "{href}"
            );
        }
    }

    #[test]
    fn a_button_label_alt_and_footer_stay_literal_beside_a_live_href() {
        let t = "{{ first_name }}";
        let blocks = [
            Block::Button {
                label: format!("Hi {t}"),
                href: "https://a.example/?e={{ email }}".into(),
            },
            Block::Image {
                src: "https://a.example/a.png".into(),
                alt: format!("alt {t}"),
            },
        ];
        let brand = BrandKit {
            footer_text: Some(format!("foot {t}")),
            ..BrandKit::default()
        };
        let saved = save(&blocks, &brand).unwrap();
        let out = send(&saved, "Ann");
        assert!(out.contains("Hi {{ first_name }}"), "{out}");
        assert!(out.contains("alt {{ first_name }}"), "{out}");
        assert!(out.contains("foot {{ first_name }}"), "{out}");
        assert!(out.contains("?e=a@b.example"), "{out}");
        assert!(!out.contains("Ann"), "{out}");
    }

    #[test]
    fn every_non_tag_field_holds_tag_syntax_as_text() {
        // Everything but a Run::Tag and the three URL fields: none of it compiles to anything but
        // one literal node.
        let t = "{{ first_name }}";
        let blocks = [
            Block::Heading {
                level: 1,
                runs: vec![text(t)],
            },
            Block::Image {
                src: "https://a.example/a.png".into(),
                alt: t.into(),
            },
            Block::Button {
                label: t.into(),
                href: "https://a.example/".into(),
            },
            Block::Columns {
                children: vec![vec![para(t)]],
            },
        ];
        let brand = BrandKit {
            footer_text: Some(t.into()),
            footer_address: Some(t.into()),
            ..BrandKit::default()
        };
        let src = to_mjml(&blocks, &brand);
        let nodes = tag::compile(&src, &Registry::from_definitions(&[])).unwrap();
        assert!(matches!(nodes.as_slice(), [Node::Text(_)]), "{nodes:?}");
    }

    #[test]
    fn a_raw_mjml_template_still_parses_the_whole_source_for_tags() {
        // The distinction is deliberate: a raw template has no Run::Tag, so its tags are in the
        // source the operator wrote.
        let src = "<mjml><mj-body><mj-section><mj-column><mj-text>Hi {{ first_name | \"there\" }}</mj-text></mj-column></mj-section></mj-body></mjml>";
        let c = compile_mjml(src, &Registry::from_definitions(&[])).unwrap();
        assert!(c.nodes.iter().any(|n| matches!(n, Node::Field { .. })));
        assert!(send(&c, "Ann").contains("Hi Ann"));
        assert!(send(&c, "").contains("Hi there"));
        // The same text in a Text BLOCK is not a tag.
        let b = save(&[para("Hi {{ first_name }}")], &BrandKit::default()).unwrap();
        assert!(!b.nodes.iter().any(|n| matches!(n, Node::Field { .. })));
    }

    #[test]
    fn the_public_entry_points_refuse_what_each_stage_refuses() {
        let reg = Registry::from_definitions(&[]);
        let tagged = |s: &str| [Block::Text { runs: vec![tag(s)] }];
        // compile: an unknown field is named, not rendered empty.
        assert!(matches!(
            compile_blocks(&tagged("{{ nope }}"), &BrandKit::default(), &reg),
            Err(CompileError::Tag(TagError::UnknownField(f))) if f == "nope"
        ));
        // mrml: its own message survives the wrapper.
        assert!(matches!(
            compile_mjml("<mjml><mj-body><mj-text>x</mj-body></mjml>", &reg),
            Err(CompileError::Mjml(MjmlError::Parse(_)))
        ));
        // classify: a tag in a position no encoding makes safe is refused at save.
        assert!(matches!(
            compile_mjml(
                "<mjml><mj-body><mj-raw><script>var a = \"{{ first_name }}\";</script></mj-raw></mj-body></mjml>",
                &reg
            ),
            Err(CompileError::DangerousTag { .. })
        ));
    }

    #[test]
    fn a_conditional_spanning_runs_works() {
        let blocks = [Block::Text {
            runs: vec![
                tag("{{# first_name }}"),
                text("named"),
                tag("{{/ first_name }}"),
                text(" always"),
            ],
        }];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        assert!(send(&saved, "x").contains("named always"));
        let out = send(&saved, "");
        assert!(!out.contains("named") && out.contains(" always"), "{out}");
    }

    #[test]
    fn text_around_a_tag_cannot_smuggle_markup_in_through_the_tag_run() {
        // A Tag run's source is operator-controlled bytes. Anything that is not the tag itself
        // is text and is escaped like any other.
        let evil = "{{ first_name }}</mj-text></mj-column></mj-section><mj-raw><script>x()</script></mj-raw>";
        let html = html_of(&[Block::Text {
            runs: vec![tag(evil)],
        }]);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;x()"), "{html}");
    }

    #[test]
    fn a_tags_fallback_is_data_and_cannot_inject_markup() {
        // The fallback holds angle brackets and an apostrophe. Compile consumes the whole tag into
        // the AST, so mrml never sees them; the value is encoded for where it lands at send.
        let blocks = [Block::Text {
            runs: vec![tag("{{ first_name | \"<script>'x\" }}")],
        }];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        let out = send(&saved, "");
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;'x"), "{out}");
    }

    #[test]
    fn braces_that_do_not_make_a_tag_are_inert_literal_text() {
        // Unterminated, lone, and adjacent to a real tag.
        let blocks = [Block::Text {
            runs: vec![
                text("a {{ oops"),
                text(" { "),
                text("{"),
                tag("{{ first_name }}"),
                text("}"),
            ],
        }];
        let saved = save(&blocks, &BrandKit::default()).unwrap();
        let out = send(&saved, "N");
        assert!(out.contains("a {{ oops {"), "{out}");
        assert!(out.contains("{N}"), "{out}");
        assert_eq!(
            saved
                .nodes
                .iter()
                .filter(|n| matches!(n, Node::Field { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn the_skeleton_itself_holds_no_merge_tag_syntax() {
        let src = to_mjml(&[], &BrandKit::default());
        let nodes = tag::compile(&src, &Registry::from_definitions(&[])).unwrap();
        assert!(matches!(nodes.as_slice(), [Node::Text(_)]), "{nodes:?}");
        // A logo is data, not template: braces in it are encoded by the URL parser.
        let src = to_mjml(
            &everything(),
            &BrandKit {
                logo_url: Some("https://a.example/{{x}}.png".into()),
                ..BrandKit::default()
            },
        );
        let nodes = tag::compile(&src, &Registry::from_definitions(&[])).unwrap();
        assert!(matches!(nodes.as_slice(), [Node::Text(_)]), "{nodes:?}");
    }

    #[test]
    fn markup_before_a_tag_in_a_tag_runs_source_is_escaped_too() {
        // The hostile text sits BEFORE the tag. Escaping only what follows a tag would leave this
        // one open: a script in the stored HTML, served on our own origin by the hosted page.
        let evil = "</mj-text><mj-raw><script>x()</script></mj-raw><mj-text>{{ first_name }}";
        let html = html_of(&[Block::Text {
            runs: vec![tag(evil)],
        }]);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;x()"), "{html}");
        // And between two tags, and in a URL field around a tag.
        let html = html_of(&[
            Block::Text {
                runs: vec![tag("{{ first_name }}<script>y()</script>{{ email }}")],
            },
            button("https://a.example/<script>z()</script>{{ first_name }}"),
        ]);
        assert!(!html.contains("<script>"), "{html}");
    }

    // ----- a literal marker is not a tag (contract section 13) --------------------------------

    #[test]
    fn a_literal_marker_in_any_field_is_refused_not_aliased_to_a_tag() {
        // `~nmmt~0~tmmn~` is the renderer's stand-in for tag 0. Typed into a field it used to
        // alias to the template's real tag 0, so a send printed the contact's name where the
        // operator typed text. REFUSED rather than escaped: an escape would lean on how mrml and
        // the HTML parser round-trip a character reference, and no legitimate template contains
        // this text, so the loud failure costs nothing.
        let m = format!("x {MARK_START}0{}", render::MARK_END);
        let reg = Registry::from_definitions(&[]);
        let brand = BrandKit {
            footer_text: Some(m.clone()),
            ..BrandKit::default()
        };
        let cases: Vec<(&str, Vec<Block>, BrandKit)> = vec![
            ("text run", vec![para(&m)], BrandKit::default()),
            (
                "image alt",
                vec![Block::Image {
                    src: "https://a.example/a.png".into(),
                    alt: m.clone(),
                }],
                BrandKit::default(),
            ),
            (
                "button label",
                vec![Block::Button {
                    label: m.clone(),
                    href: "https://a.example/".into(),
                }],
                BrandKit::default(),
            ),
            (
                "button href",
                vec![button(&format!("https://a.example/{m}"))],
                BrandKit::default(),
            ),
            (
                "tag run, around the tag",
                vec![Block::Text {
                    runs: vec![tag(&format!("{m}{{{{ first_name }}}}"))],
                }],
                BrandKit::default(),
            ),
            ("footer", vec![para("x")], brand),
        ];
        for (name, blocks, brand) in cases {
            assert_eq!(
                compile_blocks(&blocks, &brand, &reg),
                Err(CompileError::ForgedMarker),
                "{name}"
            );
        }
        assert_eq!(
            compile_mjml(
                &format!("<mjml><mj-body><mj-text>{m}</mj-text></mj-body></mjml>"),
                &reg
            ),
            Err(CompileError::ForgedMarker)
        );
        // A tilde on its own, and a marker inside a tag's FALLBACK (data in the AST, never
        // emitted as MJML and never rescanned), are not refused.
        compile_blocks(&[para("~ /~user ~nmm")], &BrandKit::default(), &reg).unwrap();
        compile_blocks(
            &[Block::Text {
                runs: vec![tag(&format!("{{{{ first_name | \"{m}\" }}}}"))],
            }],
            &BrandKit::default(),
            &reg,
        )
        .unwrap();
    }

    // ----- URLs are R3's to judge -------------------------------------------------------------

    #[test]
    fn an_operator_written_dangerous_scheme_is_neutralised_by_r3_in_every_url_position() {
        let blocks = [
            button("javascript:alert(1)"),
            Block::Image {
                src: "javascript:alert(2)".into(),
                alt: "a".into(),
            },
            Block::Text {
                runs: vec![Run::Link {
                    href: "JaVaScRiPt:alert(3)".into(),
                    text: "l".into(),
                    marks: vec![],
                }],
            },
        ];
        let out = send(&save(&blocks, &BrandKit::default()).unwrap(), "");
        assert!(!out.to_lowercase().contains("javascript"), "{out}");
        assert!(!out.contains("alert("), "{out}");
    }

    #[test]
    fn attribute_values_cannot_close_their_quotes() {
        let blocks = [
            Block::Image {
                src: "https://a.example/x.png".into(),
                alt: "\" onerror=\"alert(1)".into(),
            },
            button("https://a.example/\" onclick=\"alert(2)"),
        ];
        let out = send(&save(&blocks, &BrandKit::default()).unwrap(), "");
        // Not merely absent: mrml drops an attribute it does not know, so absence alone would
        // pass with the quotes unescaped. The text has to arrive intact, as DATA inside the value.
        assert!(
            out.contains("alt=\"&#34; onerror=&#34;alert(1)\""),
            "alt did not round-trip: {out}"
        );
        assert!(
            out.contains("href=\"https://a.example/%22%20onclick=%22alert(2)\""),
            "href did not round-trip: {out}"
        );
    }

    #[test]
    fn marks_close_in_the_reverse_order_they_open() {
        let both = Block::Text {
            runs: vec![Run::Text {
                text: "both".into(),
                marks: vec![Mark::Italic, Mark::Bold],
            }],
        };
        // mrml re-serialises mis-nested tags into well-formed ones, so pin the source as well.
        let src = to_mjml(std::slice::from_ref(&both), &BrandKit::default());
        assert!(src.contains("<strong><em>both</em></strong>"), "{src}");
        let html = html_of(&[both]);
        // Bold outermost, whatever order the wire lists them in; never `</strong></em>`.
        assert!(html.contains("<strong><em>both</em></strong>"), "{html}");
        let link = Block::Text {
            runs: vec![Run::Link {
                href: "https://a.example/".into(),
                text: "l".into(),
                marks: vec![Mark::Bold, Mark::Italic],
            }],
        };
        assert!(html_of(&[link]).contains("<strong><em>l</em></strong></a>"));
    }

    #[test]
    fn control_characters_in_an_attribute_value_become_spaces() {
        let blocks = [Block::Image {
            src: "https://a.example/a.png".into(),
            alt: "a\tb\nc\r\u{7f}d".into(),
        }];
        let html = html_of(&blocks);
        assert!(html.contains("alt=\"a b c  d\""), "{html}");
    }

    #[test]
    fn a_tag_mrml_drops_is_refused_at_save_not_shipped_missing() {
        // mrml silently discards an attribute it does not know; the tag in it would vanish from
        // every recipient's mail.
        let src = "<mjml><mj-body><mj-section><mj-column><mj-text bogus=\"{{ first_name }}\">x</mj-text></mj-column></mj-section></mj-body></mjml>";
        assert!(matches!(
            compile_mjml(src, &Registry::from_definitions(&[])),
            Err(CompileError::Render(RenderError::TagLost { .. }))
        ));
    }

    #[test]
    fn compile_errors_read_as_sentences() {
        let reg = Registry::from_definitions(&[]);
        let err = |b: &[Block]| compile_blocks(b, &BrandKit::default(), &reg).unwrap_err();
        let unknown = err(&[Block::Text {
            runs: vec![tag("{{ nope }}")],
        }]);
        assert_eq!(
            unknown.to_string(),
            "a merge tag names the field \"nope\", which does not exist"
        );
        let host = err(&[button("https://{{ first_name }}.example.com/")]).to_string();
        assert!(host.contains("scheme or host"), "{host}");
        let lost = compile_mjml(
            "<mjml><mj-body><mj-section><mj-column><mj-text bogus=\"{{ first_name }}\">x</mj-text></mj-column></mj-section></mj-body></mjml>",
            &reg,
        )
        .unwrap_err()
        .to_string();
        // Each render failure says its own thing; none is the dangerous-position message.
        assert!(
            lost.contains("dropped") && !lost.contains("no encoding"),
            "{lost}"
        );
        assert!(!unknown.to_string().contains("UnknownField"));
        let forged = CompileError::ForgedMarker.to_string();
        assert!(
            forged.contains(MARK_START) && forged.contains("remove it"),
            "{forged}"
        );
        for (e, word) in [
            (
                RenderError::BlockDuplicated { index: 0, count: 2 },
                "more than once",
            ),
            (
                RenderError::BlockStraddlesTag { index: 0 },
                "middle of an HTML tag",
            ),
            (RenderError::BlockUnbalanced { index: 0 }, "nested"),
        ] {
            let m = CompileError::Render(e).to_string();
            assert!(m.contains(word) && !m.contains("no encoding"), "{m}");
        }
    }
}
