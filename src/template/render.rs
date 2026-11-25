// SPDX-License-Identifier: AGPL-3.0-only

//! Rendering a compiled template for one contact, and the context classification that decides how
//! each substituted value is encoded (spec §5.3).
//!
//! The product ships no general template language, so there is nothing to escape *from*. This
//! module is the other half: making sure a *value* cannot break out of the context it lands in.
//! Two rules decide the design.
//!
//! **Classification runs over the HTML `mrml` produced, never over the MJML source.** `mrml`
//! rewrites attributes — an `mj-text color="…"` becomes `style="color:…"` on a `<div>` — so a
//! position classified in the MJML may not exist in the output, and a document that was
//! classified is then not the document that is sent. The compile step therefore renders
//! [`marked`] through `mrml` and hands the result to [`classify`].
//!
//! **A tag in a dangerous context is refused, never escaped.** Inside `<script>`, inside a
//! comment, in a tag name, in an attribute name, in an event-handler attribute: encoding does not
//! make those positions safe, so the answer is [`RenderError::DangerousContext`] and no template.
//!
//! The classification is driven by `html5ever`'s HTML5 tokenizer, with the tree-builder feedback
//! that makes `<script>` and `<style>` contents raw text, because the question being asked is what
//! a *browser* will do with this document — a hand-rolled scanner answers a different question.
//! Rendering re-serialises the token stream, which is what forces every attribute into double
//! quotes and what lets a URL attribute be assembled and checked as one whole URL.

use std::cell::RefCell;
use std::fmt::Write as _;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Doctype, EndTag, Tag, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use serde_json::Value;

use super::tag::{Filter, Node};

/// A merge tag's stand-in in the HTML handed to `mrml` and read back by [`classify`].
///
/// Printable ASCII, and that is not a style choice: `mrml` runs an attribute value through
/// `char::escape_debug`, so a private-use or control character survives in TEXT and comes back out
/// of an attribute as the literal seven characters `\u{e000}`. A marker that only survives half
/// the document is a merge tag that silently disappears from every `href` — which is why
/// `an_ordinary_merge_tag_survives_a_real_mrml_document` renders through real `mrml` rather than
/// trusting that markers are inert.
///
/// shortcut: a template whose own literal text contains this sequence would have it read as a
/// marker. Only the operator can write that, and the damage is to their own template; give the
/// compile path a rejection for it if it ever happens in practice.
pub const MARK_START: &str = "~nmmt~";
/// Closes a [`MARK_START`]; the digits between the two are the tag's index.
pub const MARK_END: &str = "~tmmn~";

/// Where a merge tag lands in the finished HTML.
///
/// `Dangerous` never comes back from [`classify`] inside an `Ok`: it is the classification that
/// refuses the template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Text,
    Attribute,
    Url,
    Css,
    Dangerous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// A merge tag sits where no encoding helps. `position` is the byte offset of its marker in
    /// the classified HTML.
    DangerousContext { position: usize },
}

/// The attributes whose value is a URL the browser will fetch or navigate to. Anything here gets
/// the whole-value URL treatment; everything else is an ordinary attribute.
const URL_ATTRS: [&str; 11] = [
    "action",
    "background",
    "cite",
    "data",
    "formaction",
    "href",
    "longdesc",
    "manifest",
    "ping",
    "poster",
    "src",
];

/// Frozen (spec §5.3). Widening this is a spec change, not an implementation choice.
const ALLOWED_SCHEMES: [&str; 4] = ["http", "https", "mailto", "tel"];

/// What a URL that does not survive the allowlist renders as.
const DEAD_LINK: &str = "#";

/// How many merge tags one document may hold. The index inside a marker is a number read out of
/// HTML derived from operator input — written as a character reference it survives the prescan and
/// reaches the classifier decoded — so it sizes nothing until it has been bounded.
const MAX_TAGS: usize = 10_000;

/// The one contact row a render reads. Borrowed, because a send walks millions of these and the
/// renderer owns none of it.
#[derive(Debug, Clone, Copy)]
pub struct Contact<'a> {
    pub email: &'a str,
    pub first_name: &'a str,
    pub last_name: &'a str,
    pub timezone: Option<&'a str>,
    /// Custom field values by key, as the `contacts.attributes` column stores them.
    pub attributes: &'a Value,
}

impl<'a> From<&'a crate::contacts::Contact> for Contact<'a> {
    fn from(c: &'a crate::contacts::Contact) -> Self {
        Contact {
            email: &c.email,
            first_name: &c.first_name,
            last_name: &c.last_name,
            timezone: c.timezone.as_deref(),
            attributes: &c.attributes,
        }
    }
}

impl Contact<'_> {
    /// The value a tag names, or `None` when it is absent or empty — the two are the same thing to
    /// a template, and `{{^ x }}` has to agree with `{{ x | "…" }}` about which it is.
    fn value(&self, key: &str) -> Option<String> {
        let v = match key {
            "email" => self.email.to_owned(),
            "first_name" => self.first_name.to_owned(),
            "last_name" => self.last_name.to_owned(),
            "timezone" => self.timezone.unwrap_or_default().to_owned(),
            _ => match self.attributes.get(key)? {
                Value::Array(a) => a.iter().filter_map(scalar).collect::<Vec<_>>().join(", "),
                other => scalar(other)?,
            },
        };
        (!v.is_empty()).then_some(v)
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// The template's literal text with every field tag replaced by a marker, every conditional branch
/// included.
///
/// This is what the compile step wraps in MJML and hands to `mrml`; [`classify`] then reads the
/// markers back out of the HTML `mrml` produced. Every branch is present because any of them may
/// be the one that renders, so all of them have to be classified — and a branch that would place a
/// tag somewhere dangerous has to be refused at save, not at send.
pub fn marked(nodes: &[Node]) -> String {
    let mut out = String::new();
    mark(nodes, None, &mut out, &mut Vec::new());
    out
}

fn mark<'a>(
    nodes: &'a [Node],
    c: Option<&Contact<'_>>,
    out: &mut String,
    fields: &mut Vec<&'a Node>,
) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Field { .. } => {
                let _ = write!(out, "{MARK_START}{}{MARK_END}", fields.len());
                fields.push(n);
            }
            Node::Present { key, body } => {
                if c.is_none_or(|c| c.value(key).is_some()) {
                    mark(body, c, out, fields);
                }
            }
            Node::Absent { key, body } => {
                if c.is_none_or(|c| c.value(key).is_none()) {
                    mark(body, c, out, fields);
                }
            }
            Node::Equals { key, value, body } => {
                if c.is_none_or(|c| c.value(key).as_deref() == Some(value.as_str())) {
                    mark(body, c, out, fields);
                }
            }
        }
    }
}

/// Where each merge tag in `html` lands, in document order.
///
/// `html` is the output of `mrml`, with [`marked`]'s markers still in it. Used at COMPILE time: an
/// `Err` here is a template that is never saved.
pub fn classify(html: &str) -> Result<Vec<Site>, RenderError> {
    scan(html, None).map(|(sites, _)| sites)
}

/// One contact's copy of the document.
///
/// A value is substituted and then written out; it is never read back, so a contact whose
/// `first_name` is the text `{{ email }}` gets that text and not somebody's address.
pub fn render_html(nodes: &[Node], contact: &Contact<'_>) -> Result<String, RenderError> {
    let mut html = String::new();
    let mut fields = Vec::new();
    mark(nodes, Some(contact), &mut html, &mut fields);
    let values: Vec<Option<String>> = fields.iter().map(|n| field_value(n, contact)).collect();
    scan(&html, Some(&values)).map(|(_, out)| out)
}

/// A field tag's value for this contact: the stored value through its filter, or the fallback.
fn field_value(node: &Node, c: &Contact<'_>) -> Option<String> {
    let Node::Field {
        key,
        filter,
        fallback,
    } = node
    else {
        return None;
    };
    match c.value(key) {
        Some(v) => Some(apply(filter.as_ref(), v)),
        None => fallback.clone(),
    }
}

fn apply(filter: Option<&Filter>, v: String) -> String {
    match filter {
        None => v,
        Some(Filter::Upper) => v.to_uppercase(),
        Some(Filter::Lower) => v.to_lowercase(),
        Some(Filter::Title) => title_case(&v),
        // The format was validated against a sample date at compile, so this only falls back when
        // the stored VALUE is not a date.
        Some(Filter::Date(fmt)) => format_date(&v, fmt).unwrap_or(v),
    }
}

fn title_case(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut start = true;
    for c in v.chars() {
        if start {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start = c.is_whitespace() || c == '-';
    }
    out
}

fn format_date(v: &str, fmt: &str) -> Option<String> {
    let d = chrono::DateTime::parse_from_rfc3339(v)
        .map(|d| d.naive_utc().date())
        .or_else(|_| chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d"))
        .ok()?;
    let mut out = String::new();
    write!(out, "{}", d.format(fmt)).ok()?;
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Markers

enum Seg<'a> {
    Lit(&'a str),
    Mark(usize),
}

/// Splits a run of text into literal and marker pieces. A `MARK_START` with no digits-then-`MARK_END`
/// after it is not a marker; it is dropped rather than carried into the output.
fn segments(s: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(MARK_START) {
        if i > 0 {
            out.push(Seg::Lit(&rest[..i]));
        }
        let after = &rest[i + MARK_START.len()..];
        match id_in(after) {
            Some((id, len)) => {
                out.push(Seg::Mark(id));
                rest = &after[len + MARK_END.len()..];
            }
            None => rest = after,
        }
    }
    if !rest.is_empty() {
        out.push(Seg::Lit(rest));
    }
    out
}

/// The digits of a marker that has just had its `MARK_START` consumed, and their byte length.
fn id_in(after: &str) -> Option<(usize, usize)> {
    let len = after.find(MARK_END)?;
    let digits = &after[..len];
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
        .map(|id| (id, len))
}

fn position_of(html: &str, id: usize) -> usize {
    html.find(&format!("{MARK_START}{id}{MARK_END}"))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Encoders

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Safe inside the double quotes this module's serialiser always writes. The quote itself and `&`
/// are the two that matter; `<`, `>` and `'` cost little and stop a sloppier downstream parser
/// from finding markup here.
fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// CSS escaping: everything outside a small literal set becomes `\<hex> `, which is inert both in
/// a `style` attribute (where the browser decodes entities FIRST, so an entity-based escape would
/// not survive) and in `<style>` raw text (where it never decodes entities at all).
///
/// shortcut: the literal set covers colours, lengths and font stacks, not `rgb(…)` — a parenthesis
/// is escaped and the value stops being a function call. Widen the set if a CSS position ever has
/// to take a function.
fn escape_css(s: &str, out: &mut String) {
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, ' ' | '#' | '%' | ',' | '.' | '-' | '_') {
            out.push(c);
        } else {
            let _ = write!(out, "\\{:x} ", c as u32);
        }
    }
}

/// Substitute, parse, canonicalise, check the scheme, re-serialise.
///
/// Never a blocklist and never a regex for the string `javascript:` — `java\tscript:`,
/// `&#106;avascript:`, `JaVaScript:`, a leading NUL and a leading newline all defeat string
/// matching, and a real URL parser reduces every one of them to the same scheme.
fn safe_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(u) if ALLOWED_SCHEMES.contains(&u.scheme()) => u.to_string(),
        // The URL itself is contact data, so only the reason is logged.
        Ok(u) => {
            tracing::warn!(
                scheme = u.scheme(),
                "merge tag produced a URL whose scheme is not allowed; link disabled"
            );
            DEAD_LINK.to_owned()
        }
        Err(e) => {
            tracing::warn!(error = %e, "merge tag produced a URL that does not parse; link disabled");
            DEAD_LINK.to_owned()
        }
    }
}

/// An attribute's site, by name.
fn attr_site(name: &str) -> Site {
    // An event handler's value is JavaScript the browser parses AFTER decoding entities, so
    // attribute encoding buys exactly nothing there; `srcdoc` is a whole HTML document.
    if name.len() > 2 && name.starts_with("on") {
        return Site::Dangerous;
    }
    match name {
        "style" => Site::Css,
        "srcdoc" => Site::Dangerous,
        n if URL_ATTRS.contains(&n) => Site::Url,
        _ => Site::Attribute,
    }
}

// ---------------------------------------------------------------------------------------------
// The tokenizer sink: classifies every marker and re-serialises the document around it.

/// Which element's contents the tokenizer is inside. Tracked here because it decides the site of a
/// text run, and only this sink knows it: the tokenizer is told the mode BY us.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Raw {
    Data,
    Rcdata,
    Style,
    Script,
    Other,
}

struct Sink<'v> {
    st: RefCell<St<'v>>,
}

struct St<'v> {
    sites: Vec<Option<Site>>,
    /// `None` when classifying: there is no contact, so nothing is substituted and `out` is scrap.
    values: Option<&'v [Option<String>]>,
    out: String,
    raw: Raw,
    /// A marker index past `MAX_TAGS`, which only a forged marker produces. Refused at the end,
    /// like any other position nobody could classify.
    forged: bool,
    /// Character tokens arrive in chunks split at `&`, `<` and NUL; a run is buffered so a marker
    /// can never be half in one chunk and half in the next.
    text: String,
}

impl<'v> St<'v> {
    fn value(&self, id: usize) -> Option<&'v str> {
        self.values?.get(id)?.as_deref()
    }

    fn record(&mut self, id: usize, site: Site) {
        if id >= MAX_TAGS {
            self.forged = true;
            return;
        }
        if self.sites.len() <= id {
            self.sites.resize(id + 1, None);
        }
        let slot = &mut self.sites[id];
        if slot.is_none() || site == Site::Dangerous {
            *slot = Some(site);
        }
    }

    fn mark_all(&mut self, s: &str, site: Site) {
        for seg in segments(s) {
            if let Seg::Mark(id) = seg {
                self.record(id, site);
            }
        }
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let raw = self.raw;
        let site = match raw {
            Raw::Data | Raw::Rcdata => Site::Text,
            Raw::Style => Site::Css,
            // Script data, and the other raw-text elements. A value here is code or markup that no
            // encoding defuses, which is the whole reason this is a refusal and not an escape.
            Raw::Script | Raw::Other => Site::Dangerous,
        };
        for seg in segments(&text) {
            match seg {
                Seg::Lit(s) => match raw {
                    Raw::Data | Raw::Rcdata => escape_text(s, &mut self.out),
                    Raw::Style | Raw::Script | Raw::Other => self.out.push_str(s),
                },
                Seg::Mark(id) => {
                    self.record(id, site);
                    if let Some(v) = self.value(id) {
                        match site {
                            Site::Text => escape_text(v, &mut self.out),
                            Site::Css => escape_css(v, &mut self.out),
                            // Dangerous: the document is refused and `out` is discarded.
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn attr_value(&mut self, value: &str, site: Site) {
        let segs = segments(value);
        for seg in &segs {
            if let Seg::Mark(id) = seg {
                self.record(*id, site);
            }
        }
        let substituted = segs.iter().any(|s| matches!(s, Seg::Mark(_)));
        if site == Site::Url && substituted {
            // The whole attribute is one URL and is checked as one: a value spliced into it decides
            // the scheme as surely as a value that is the entire attribute, so neither may be
            // validated on its own.
            if self.values.is_some() {
                let mut url = String::new();
                for seg in &segs {
                    match seg {
                        Seg::Lit(l) => url.push_str(l),
                        Seg::Mark(id) => url.push_str(self.value(*id).unwrap_or_default()),
                    }
                }
                escape_attr(&safe_url(&url), &mut self.out);
            }
            return;
        }
        for seg in &segs {
            match seg {
                Seg::Lit(l) => escape_attr(l, &mut self.out),
                Seg::Mark(id) => {
                    if let Some(v) = self.value(*id) {
                        match site {
                            Site::Css => escape_css(v, &mut self.out),
                            Site::Attribute | Site::Url => escape_attr(v, &mut self.out),
                            Site::Text | Site::Dangerous => {}
                        }
                    }
                }
            }
        }
    }

    fn tag(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;
        // A marker can continue a tag name (`<a{{ x }}>`): it is not a letter, so it cannot START
        // one, but the tokenizer happily reads it as part of a name that began with a letter.
        self.mark_all(name, Site::Dangerous);
        for a in &tag.attrs {
            self.mark_all(&a.name.local, Site::Dangerous);
        }

        if tag.kind == EndTag {
            // An end tag's attributes are ignored by the spec, but a marker in one still has to be
            // accounted for or the fail-closed check would be the only thing catching it.
            for a in &tag.attrs {
                self.mark_all(&a.value, Site::Dangerous);
            }
            self.raw = Raw::Data;
            let _ = write!(self.out, "</{name}>");
            return TokenSinkResult::Continue;
        }

        self.out.push('<');
        self.out.push_str(name);
        for a in &tag.attrs {
            let n: &str = &a.name.local;
            self.out.push(' ');
            self.out.push_str(n);
            // Every attribute is re-serialised with double quotes, whatever the source wrote, so
            // the encoder never has to guess which character ends this value.
            self.out.push_str("=\"");
            self.attr_value(&a.value, attr_site(n));
            self.out.push('"');
        }
        if tag.self_closing {
            self.out.push_str(" /");
        }
        self.out.push('>');

        let (raw, feedback) = match name {
            "script" => (Raw::Script, Some(RawKind::ScriptData)),
            "style" => (Raw::Style, Some(RawKind::Rawtext)),
            "xmp" | "iframe" | "noembed" | "noframes" => (Raw::Other, Some(RawKind::Rawtext)),
            "title" | "textarea" => (Raw::Rcdata, Some(RawKind::Rcdata)),
            _ => (Raw::Data, None),
        };
        self.raw = raw;
        match feedback {
            Some(k) => TokenSinkResult::RawData(k),
            None => TokenSinkResult::Continue,
        }
    }

    fn comment(&mut self, text: &str) {
        // A comment is not inert: `<!--{{ x }}-->` with a value holding `-->` resumes markup, and
        // email HTML is full of `<!--[if mso]>` blocks whose contents a browser does parse.
        self.mark_all(text, Site::Dangerous);
        let _ = write!(self.out, "<!--{text}-->");
    }

    fn doctype(&mut self, d: &Doctype) {
        for part in [&d.name, &d.public_id, &d.system_id].into_iter().flatten() {
            self.mark_all(part, Site::Dangerous);
        }
        self.out.push_str("<!DOCTYPE ");
        self.out.push_str(d.name.as_deref().unwrap_or("html"));
        // The quote is dropped rather than escaped: a doctype has no escape syntax to use.
        let quoted = |s: &StrTendril| s.replace('"', "");
        if let Some(p) = &d.public_id {
            let _ = write!(self.out, " PUBLIC \"{}\"", quoted(p));
        }
        if let Some(s) = &d.system_id {
            let _ = write!(self.out, " \"{}\"", quoted(s));
        }
        self.out.push('>');
    }
}

impl TokenSink for Sink<'_> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut st = self.st.borrow_mut();
        match token {
            Token::CharacterTokens(t) => st.text.push_str(&t),
            Token::NullCharacterToken => st.text.push('\u{FFFD}'),
            Token::TagToken(tag) => {
                st.flush_text();
                return st.tag(&tag);
            }
            Token::CommentToken(t) => {
                st.flush_text();
                st.comment(&t);
            }
            Token::DoctypeToken(d) => {
                st.flush_text();
                st.doctype(&d);
            }
            Token::ParseError(_) | Token::EOFToken => st.flush_text(),
        }
        TokenSinkResult::Continue
    }
}

/// Classify every marker in `html`, and — when `values` is given — re-serialise the document with
/// each marker replaced by its encoded value.
fn scan(html: &str, values: Option<&[Option<String>]>) -> Result<(Vec<Site>, String), RenderError> {
    let mut sites: Vec<Option<Site>> = Vec::new();
    let bytes = html.as_bytes();
    for (i, _) in html.match_indices(MARK_START) {
        let Some((id, _)) = id_in(&html[i + MARK_START.len()..]) else {
            continue;
        };
        if id >= MAX_TAGS {
            return Err(RenderError::DangerousContext { position: i });
        }
        if sites.len() <= id {
            sites.resize(id + 1, None);
        }
        // `<{{ tag }}>`: the tokenizer reads this as TEXT, because a marker is not an ASCII letter
        // and so does not open a tag — but the `<` is already in the template, and a value dropped
        // straight after it is a tag name whatever the tokenizer calls the position.
        if i > 0 && bytes[i - 1] == b'<' {
            sites[id] = Some(Site::Dangerous);
        }
    }

    let tok = Tokenizer::new(
        Sink {
            st: RefCell::new(St {
                sites,
                values,
                out: String::new(),
                raw: Raw::Data,
                forged: false,
                text: String::new(),
            }),
        },
        TokenizerOpts::default(),
    );
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tok.feed(&input);
    tok.end();

    let mut st = tok.sink.st.borrow_mut();
    st.flush_text();
    // Fail closed. A marker the tokenizer never handed back is a position nobody classified, and
    // an unclassified position is the one case where "it is probably fine" is how this ships a
    // vulnerability.
    let sites: Vec<Site> = st
        .sites
        .iter()
        .map(|s| s.unwrap_or(Site::Dangerous))
        .collect();
    if let Some(id) = sites.iter().position(|s| *s == Site::Dangerous) {
        return Err(RenderError::DangerousContext {
            position: position_of(html, id),
        });
    }
    if st.forged {
        return Err(RenderError::DangerousContext { position: 0 });
    }
    Ok((sites, std::mem::take(&mut st.out)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::custom_fields::{Definition, FieldKind};
    use crate::template::tag::{Registry, compile};

    fn def(key: &str) -> Definition {
        Definition {
            id: uuid::Uuid::nil(),
            key: key.into(),
            label: key.into(),
            kind: FieldKind::Text,
            options: vec![],
            archived_at: None,
        }
    }

    fn nodes(src: &str) -> Vec<Node> {
        let defs = ["link", "colour", "bio"].map(def);
        compile(src, &Registry::from_definitions(&defs)).expect("the template must compile")
    }

    /// What the compile step does: mark the tags, (optionally render through `mrml`), classify.
    fn sites(src: &str) -> Result<Vec<Site>, RenderError> {
        classify(&marked(&nodes(src)))
    }

    fn render(src: &str, first_name: &str, attrs: Value) -> Result<String, RenderError> {
        let c = Contact {
            email: "ada@example.com",
            first_name,
            last_name: "Lovelace",
            timezone: None,
            attributes: &attrs,
        };
        render_html(&nodes(src), &c)
    }

    fn ok(src: &str, first_name: &str, attrs: Value) -> String {
        render(src, first_name, attrs).expect("the template must render")
    }

    #[test]
    fn text_position_is_entity_encoded() {
        assert_eq!(sites("<p>{{ first_name }}</p>").unwrap(), vec![Site::Text]);
        assert_eq!(
            ok("<p>{{ first_name }}</p>", "<b>Ada & \"co\"</b>", json!({})),
            "<p>&lt;b&gt;Ada &amp; \"co\"&lt;/b&gt;</p>"
        );
        // The classic: a value that is a whole script element stays text.
        assert_eq!(
            ok(
                "<p>{{ first_name }}</p>",
                "<script>alert(1)</script>",
                json!({})
            ),
            "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
        );
    }

    #[test]
    fn attribute_position_is_attribute_encoded_with_forced_double_quotes() {
        assert_eq!(
            sites("<img alt=\"{{ first_name }}\">").unwrap(),
            vec![Site::Attribute]
        );
        let out = ok(
            "<img alt=\"{{ first_name }}\">",
            "x\" onerror=\"alert(1)",
            json!({}),
        );
        assert_eq!(out, "<img alt=\"x&#34; onerror=&#34;alert(1)\">");
        // An UNQUOTED attribute in the source is re-serialised with double quotes, so the value
        // cannot end the attribute by holding a space.
        let out = ok(
            "<img alt={{ first_name }}>",
            "x onerror=alert(1)",
            json!({}),
        );
        assert_eq!(out, "<img alt=\"x onerror=alert(1)\">");
        // Single quotes too: the value's own `'` is encoded whatever the source quoted with.
        let out = ok("<img alt='{{ first_name }}'>", "x' onerror='y", json!({}));
        assert_eq!(out, "<img alt=\"x&#39; onerror=&#39;y\">");
    }

    #[test]
    fn url_position_is_percent_encoded() {
        assert_eq!(
            sites("<a href=\"{{ link }}\">x</a>").unwrap(),
            vec![Site::Url]
        );
        let out = ok(
            "<a href=\"{{ link }}\">x</a>",
            "Ada",
            json!({ "link": "https://example.com/a b\"c<d?q=1 2" }),
        );
        assert_eq!(
            out,
            "<a href=\"https://example.com/a%20b%22c%3Cd?q=1%202\">x</a>"
        );
    }

    #[test]
    fn a_tag_inside_script_fails_to_compile() {
        assert!(matches!(
            sites("<script>var n = \"{{ first_name }}\";</script>"),
            Err(RenderError::DangerousContext { .. })
        ));
        // And the same value in a style element is a CSS position, not a refusal.
        assert_eq!(
            sites("<style>.x{color:{{ colour }}}</style>").unwrap(),
            vec![Site::Css]
        );
    }

    #[test]
    fn a_tag_inside_an_html_comment_fails_to_compile() {
        assert!(matches!(
            sites("<p>a</p><!-- hello {{ first_name }} -->"),
            Err(RenderError::DangerousContext { .. })
        ));
        // Email HTML's conditional comments are comments like any other.
        assert!(matches!(
            sites("<!--[if mso]>{{ first_name }}<![endif]-->"),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    #[test]
    fn a_tag_in_a_tag_name_fails_to_compile() {
        for src in [
            "<{{ first_name }} href=\"x\">y",
            "<a{{ first_name }} href=\"x\">y</a>",
            "</{{ first_name }}>",
        ] {
            assert!(
                matches!(sites(src), Err(RenderError::DangerousContext { .. })),
                "{src}: {:?}",
                sites(src)
            );
        }
    }

    #[test]
    fn a_tag_in_an_attribute_name_fails_to_compile() {
        for src in [
            "<a {{ first_name }}=\"x\">y</a>",
            "<a href=\"x\" {{ first_name }}>y</a>",
            "<a href=\"x\" on{{ first_name }}=\"1\">y</a>",
            // An end tag's attributes are dropped by the spec, so the value would silently vanish
            // — but nothing may be left unclassified.
            "<a href=\"x\">y</a {{ first_name }}=\"z\">",
            // An event handler's value is JavaScript, decoded before it is parsed.
            "<a href=\"x\" onclick=\"f('{{ first_name }}')\">y</a>",
        ] {
            assert!(
                matches!(sites(src), Err(RenderError::DangerousContext { .. })),
                "{src}: {:?}",
                sites(src)
            );
        }
    }

    #[test]
    fn obfuscated_javascript_schemes_all_render_hash() {
        for raw in [
            "javascript:alert(1)",
            "java\tscript:alert(1)",
            "&#106;avascript:alert(1)",
            "JaVaScript:alert(1)",
            "\0javascript:alert(1)",
            "\njavascript:alert(1)",
            "\u{0}\u{9}JaVa\nScRiPt\t:alert(1)",
            "data:text/html;base64,PHNjcmlwdD4=",
            "vbscript:msgbox(1)",
        ] {
            let out = ok(
                "<a href=\"{{ link }}\">x</a>",
                "Ada",
                json!({ "link": raw }),
            );
            assert_eq!(out, "<a href=\"#\">x</a>", "{raw:?}");
        }
        // The allowlist is not a wall: the four schemes that are on it still work.
        for raw in [
            "https://example.com/",
            "http://example.com/",
            "mailto:ada@example.com",
            "tel:+15551234567",
        ] {
            let out = ok(
                "<a href=\"{{ link }}\">x</a>",
                "Ada",
                json!({ "link": raw }),
            );
            assert!(out.contains(raw), "{raw}: {out}");
        }
        // A value spliced into the MIDDLE of a URL is checked as part of the whole URL too.
        let out = ok(
            "<a href=\"https://example.com/{{ link }}\">x</a>",
            "Ada",
            json!({ "link": "a\"><script>alert(1)</script>" }),
        );
        assert_eq!(
            out,
            "<a href=\"https://example.com/a%22%3E%3Cscript%3Ealert(1)%3C/script%3E\">x</a>"
        );
    }

    #[test]
    fn a_conditional_branch_obeys_the_same_rules() {
        let src = "{{# link }}<a href=\"{{ link }}\">{{ first_name }}</a>{{/ link }}\
                   {{^ link }}<p>{{ first_name }}</p>{{/ link }}";
        assert_eq!(
            ok(src, "<b>", json!({ "link": "javascript:alert(1)" })),
            "<a href=\"#\">&lt;b&gt;</a>"
        );
        assert_eq!(ok(src, "<b>", json!({})), "<p>&lt;b&gt;</p>");
        // A branch that would put a tag somewhere dangerous is refused at COMPILE, even though no
        // contact has chosen that branch yet.
        assert!(matches!(
            sites("{{# link }}<script>{{ first_name }}</script>{{/ link }}"),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    #[test]
    fn classification_runs_on_mrml_output_not_on_mjml() {
        let src = "<mjml><mj-body><mj-section><mj-column>\
                   <mj-text color=\"{{ colour }}\">hi</mj-text>\
                   </mj-column></mj-section></mj-body></mjml>";
        let mjml = marked(&nodes(src));
        // In the MJML source the tag sits in an ordinary attribute of a custom element.
        assert_eq!(classify(&mjml).unwrap(), vec![Site::Attribute]);
        // `mrml` moves it into a `style` attribute on a real `div`, which is a CSS position — so
        // classifying the source would have encoded it for the wrong context entirely.
        let html = mrml::parse(&mjml)
            .expect("mjml parses")
            .element
            .render(&mrml::prelude::render::RenderOptions::default())
            .expect("mjml renders");
        assert!(html.contains(MARK_START), "the marker must survive mrml");
        assert_eq!(classify(&html).unwrap(), vec![Site::Css]);
    }

    /// The whole compile path on a real document: `mrml` wraps body content in `<!--[if mso]>`
    /// blocks and in a `<style>` head, so a tag in ordinary text has to come back `Text` and not
    /// get caught by the comment or raw-text rules.
    #[test]
    fn an_ordinary_merge_tag_survives_a_real_mrml_document() {
        let src = "<mjml><mj-body><mj-section><mj-column>\
                   <mj-text>Hi {{ first_name }}</mj-text>\
                   <mj-button href=\"{{ link }}\">Go</mj-button>\
                   </mj-column></mj-section></mj-body></mjml>";
        let html = mrml::parse(marked(&nodes(src)))
            .expect("mjml parses")
            .element
            .render(&mrml::prelude::render::RenderOptions::default())
            .expect("mjml renders");
        for l in html
            .lines()
            .filter(|l| l.contains("href") || l.contains(MARK_START))
        {
            println!("LINE {l}");
        }
        assert_eq!(classify(&html).unwrap(), vec![Site::Text, Site::Url]);
    }

    /// A marker index is a number in HTML that came from operator input. Written as a character
    /// reference it slips past the raw prescan, so the classifier has to bound it rather than
    /// resize a vector to fit.

    #[test]
    fn a_forged_marker_index_is_refused_not_allocated_for() {
        // `~` written as `&#126;`: the raw prescan sees character references, the tokenizer sees
        // a marker with an index that would size a vector.
        let huge =
            format!("<p>&#126;nmmt&#126;4294967295&#126;tmmn&#126;{MARK_START}0{MARK_END}</p>");
        assert!(matches!(
            classify(&huge),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    #[test]
    fn unicode_survives_text_and_punycodes_an_idn_host() {
        // U+202E right-to-left override, U+200D zero-width joiner, U+0301 combining acute.
        let name = "a\u{202E}b\u{200D}c\u{0301}d";
        assert_eq!(
            ok("<p>{{ first_name }}</p>", name, json!({})),
            format!("<p>{name}</p>")
        );
        let out = ok(
            "<a href=\"{{ link }}\">x</a>",
            "Ada",
            json!({ "link": "https://exämple.com/ü?q=é" }),
        );
        assert_eq!(
            out,
            "<a href=\"https://xn--exmple-cua.com/%C3%BC?q=%C3%A9\">x</a>"
        );
    }

    /// R2 could only test this against a stand-in evaluator, because no renderer existed. This is
    /// the real guard: if `render_html` re-scanned what it had just written, a contact who sets a
    /// name would be choosing which of somebody else's fields the email shows.
    #[test]
    fn a_substituted_value_is_never_re_parsed() {
        let src = "Hi {{ first_name }}, we have {{ email }} on file.";
        assert_eq!(
            ok(src, "{{ email }}", json!({})),
            "Hi {{ email }}, we have ada@example.com on file."
        );
        // A value that is a block opener, or names a field that does not exist, is equally inert.
        assert_eq!(
            ok(src, "{{# link }}{{ nope }}", json!({})),
            "Hi {{# link }}{{ nope }}, we have ada@example.com on file."
        );
        // Nor can a value forge a marker and steal another tag's slot.
        let forged = format!("{MARK_START}1{MARK_END}");
        assert_eq!(
            ok(src, &forged, json!({})),
            format!("Hi {forged}, we have ada@example.com on file.")
        );
    }

    #[test]
    fn a_filter_runs_on_the_value_and_the_result_is_still_encoded() {
        assert_eq!(
            ok("<p>{{ first_name | upper }}</p>", "<ada>", json!({})),
            "<p>&lt;ADA&gt;</p>"
        );
        assert_eq!(
            ok("<p>{{ first_name | title }}</p>", "ada lovelace", json!({})),
            "<p>Ada Lovelace</p>"
        );
        // An absent value takes the fallback, which is encoded like any other value.
        assert_eq!(
            ok("<p>{{ bio | \"none\" }}</p>", "Ada", json!({})),
            "<p>none</p>"
        );
        assert_eq!(
            ok(
                "<p>{{ bio | date:%Y-%m-%d }}</p>",
                "Ada",
                json!({"bio": "2024-03-05T06:07:08Z"})
            ),
            "<p>2024-03-05</p>"
        );
    }
}
