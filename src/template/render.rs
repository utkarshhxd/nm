// SPDX-License-Identifier: AGPL-3.0-only

//! Rendering a compiled template for one contact, and the context classification that decides how
//! each substituted value is encoded (spec §5.3).
//!
//! The product ships no general template language, so there is nothing to escape *from*. This
//! module is the other half: making sure a *value* cannot break out of the context it lands in.
//!
//! # The pipeline, which is not negotiable
//!
//! ```text
//! save:  AST --marked()--> marked MJML --mrml--> marked HTML --classify()--> refuse or store
//! send:  stored marked HTML + AST + contact --substitute()--> the message
//! ```
//!
//! `mrml` runs **once, at save**, and the marked HTML it produced is what gets stored. Every send
//! substitutes into *that* document and never runs `mrml` again. Two reasons, in order:
//!
//! 1. **Correctness.** `mrml` rewrites attributes — `mj-text color="…"` becomes `style="color:…"`
//!    on a `div` — so a position classified in the MJML may not exist in the output, and the
//!    encoder a value needs is decided by where it lands in the HTML, not in the source. Wiring it
//!    the other way means the document that was classified is not the document that is sent, and
//!    the gap is a CSS injection: `color` is an ordinary attribute in the MJML and a CSS context in
//!    the HTML. `classification_runs_on_mrml_output_not_on_mjml` is the proof those two disagree.
//! 2. Speed, a distant second: it takes a full MJML render off a path that walks millions of rows.
//!
//! # The two rules
//!
//! **Classification runs over the HTML `mrml` produced, never over the MJML source**, and every
//! tag `marked()` emitted must still be there. A merge tag that `mrml` dropped is a tag that is
//! silently gone from every recipient's mail, so [`classify`] is told how many to expect and
//! refuses when one is missing.
//!
//! **A tag in a dangerous context is refused, never escaped.** Inside `<script>`, inside a comment,
//! in a tag name, in an attribute name, in an event-handler attribute, in a `meta refresh`, and in
//! the scheme or host of a URL: encoding does not make those positions safe, so the answer is
//! [`RenderError::DangerousContext`] and no template.
//!
//! The classification is driven by `html5ever`'s HTML5 tokenizer, with the tree-builder feedback
//! that makes `<script>` and `<style>` contents raw text, because the question being asked is what
//! a *browser* will do with this document — a hand-rolled scanner answers a different question.
//! Rendering re-serialises the token stream, which is what forces every attribute into double
//! quotes.

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
/// marker. Only the operator can write one — `mrml` copies the suffix verbatim, and a contact
/// value is never re-scanned — but the damage is NOT confined to the forger's own template: a
/// forged marker aliasing a conditional's index would, unchecked, turn the pair back into "just a
/// field" and a branch meant to be hidden would render to every recipient. `scan` refuses an index
/// used in both forms for that reason. Give the compile path a rejection for a literal marker in
/// template text if it ever happens in practice.
pub const MARK_START: &str = "~nmmt~";
/// Closes a [`MARK_START`]; the digits between the two are the tag's index.
pub const MARK_END: &str = "~tmmn~";
/// The spelling and index scheme of the markers in a stored document.
///
/// **Bump this whenever either changes**, and that means the marker text itself, the suffixes, or
/// what an index counts. The marked HTML is stored at save and substituted into at send, possibly
/// months later and certainly after deploys, so a document carries a format its code may no longer
/// speak. The caller that stores the HTML stores this number beside it and refuses to substitute
/// into a version it does not recognise, rather than guessing — guessing means markers read as
/// literal text in a sent message, or worse, a conditional whose boundaries are not understood.
///
/// The version belongs to the stored blob, not inside the markers: putting it in the markers would
/// make it part of the thing being versioned.
///
/// Two changes have already happened, neither of which reached storage, which is the only reason
/// this is a constant and not a migration:
///
/// 1. `U+E000`/`U+E001` private-use delimiters. `mrml` runs attribute values through
///    `char::escape_debug`, so they survived in text and came out of an `href` as the literal
///    `\u{e000}` — a merge tag that silently vanished from every link.
/// 2. `~nmmt~<index>~tmmn~`, with a `b` suffix marking a conditional's boundary. A conditional's
///    two markers carried unrelated indices, so their pairing was not derivable from the HTML and
///    a reordered or crossed pair could not be refused.
/// 3. The present scheme: `o` and `c` suffixes, and a conditional is ONE index written twice, so
///    a close names the open it belongs to.
pub const MARKER_FORMAT_VERSION: u32 = 3;

/// Follow the index when the marker opens or closes a conditional rather than standing for a
/// field. They are in the marker rather than in a parameter so that [`classify`] can tell the
/// three apart from the stored HTML alone, with nothing extra to keep in step.
///
/// A conditional's two markers share ONE index, so the pair is explicit: a close marker names the
/// open it belongs to, and [`scan`] can check ordering and nesting without being handed a map.
const MARK_OPEN: &str = "o";
const MARK_CLOSE: &str = "c";

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
    /// A merge tag that [`marked`] emitted is not in the classified HTML at all — `mrml` dropped
    /// the attribute or the element that held it. Saving this template would publish a message
    /// with the tag silently missing, so it is refused instead.
    TagLost { index: usize },
    /// A conditional's boundary markers did not survive exactly once each, so the renderer cannot
    /// tell which copy delimits the branch. Also covers an index used in both boundary and field
    /// form, which would otherwise hide the pair from this check entirely.
    ///
    /// A duplicated FIELD marker is fine — every copy is given the same value, and `mrml`
    /// legitimately emits an `mj-button` href twice. A duplicated BOUNDARY is not: suppressing a
    /// branch means rewinding the output between a matching pair, and with three of them the
    /// wrong range of a sent message gets dropped, silently, which is the one failure in this
    /// module that would not announce itself.
    BlockDuplicated { index: usize, count: usize },
    /// A conditional's two markers do not bracket a whole region of the document: one is inside an
    /// attribute value and the other is not, or they are in different attributes.
    ///
    /// Dropping a branch means rewinding the output between the pair. When they straddle a tag the
    /// rewind lands between `<` and `>`, the serialiser keeps writing, and everything after it is
    /// parsed as attributes of an unterminated tag — so a value encoded for the context it was
    /// classified in ends up in a different one. That is the whole defect this module exists to
    /// prevent, arriving by way of the conditional.
    BlockStraddlesTag { index: usize },
    /// A conditional's markers are out of order, unmatched, or not properly nested.
    ///
    /// `mrml 6.0.1` does not reorder markers. That is an observation about one version, and this
    /// module has been bitten three times by exactly that kind of observation, so the pairing is
    /// required rather than assumed.
    BlockUnbalanced { index: usize },
    /// A send rendition's system footer sits inside a conditional, so every recipient the branch
    /// is not taken for would get no unsubscribe link and no postal address.
    FooterInBlock,
}

/// The attributes whose value is a URL the browser will fetch or navigate to. Anything here gets
/// the URL treatment; everything else is an ordinary attribute.
///
/// `xlink:href` is in the list because the HTML tokenizer keeps the prefix in the local name, and
/// `<svg><a xlink:href="…">` is reachable through `mj-raw` — inert in most inboxes, live on the
/// hosted view-in-browser page.
const URL_ATTRS: [&str; 12] = [
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
    "xlink:href",
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
    /// This message's unsubscribe token, which fills the system footer's
    /// [`UNSUBSCRIBE_TOKEN`](super::UNSUBSCRIBE_TOKEN) slot and nothing else: no operator registry
    /// can name that slot. There is no `From<&contacts::Contact>`, because a default here would be
    /// an unsubscribe link with no token in it, sent without a word.
    pub unsubscribe_token: &'a str,
    /// This message's tracking token, which fills the footer's
    /// [`TRACKING_TOKEN`](super::TRACKING_TOKEN) slot and every tracked link. `None` for a test
    /// send or a preview: links are left as written and the pixel's slot renders `test`.
    pub tracking_token: Option<&'a str>,
}

impl Contact<'_> {
    /// The value a tag names, or `None` when it is absent or empty — the two are the same thing to
    /// a template, and `{{^ x }}` has to agree with `{{ x | "…" }}` about which it is.
    pub(super) fn value(&self, key: &str) -> Option<String> {
        let v = match key {
            "email" => self.email.to_owned(),
            "first_name" => self.first_name.to_owned(),
            "last_name" => self.last_name.to_owned(),
            "timezone" => self.timezone.unwrap_or_default().to_owned(),
            // Answered before `attributes`, so a contact value stored under either key never
            // fills the slot.
            super::UNSUBSCRIBE_TOKEN => self.unsubscribe_token.to_owned(),
            super::TRACKING_TOKEN => self.tracking_token.unwrap_or("test").to_owned(),
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

// ---------------------------------------------------------------------------------------------
// Marking: one traversal, used by both ends of the pipeline

/// What the marker at a given index stands for. The traversal that writes the markers is the one
/// that builds this list, so the indices cannot drift apart.
enum Slot<'a> {
    Field(&'a Node),
    /// A conditional. It writes TWO markers — an open and a close sharing this one index — so the
    /// pair is self-describing in the HTML.
    Block(&'a Node),
}

/// What a marker is replaced by at send time, by index.
enum Fill {
    Value(Option<String>),
    /// `true` when this contact takes the branch; `false` suppresses everything between the two
    /// markers.
    Block(bool),
}

/// Which of the three forms a marker is in. Read back out of the marker itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Field,
    Open,
    Close,
}

/// The template's literal text with every tag replaced by a marker, and the number of markers.
///
/// Every conditional branch is present, because any of them may be the one that renders: all of
/// them have to be classified, and a branch that would place a tag somewhere dangerous has to be
/// refused at save rather than at send. Which branch a contact actually takes is decided by
/// [`substitute`], against this same document.
///
/// This is what the compile step wraps in MJML and hands to `mrml`. The count goes to [`classify`]
/// so a tag `mrml` dropped cannot pass for a template that never had one.
pub fn marked(nodes: &[Node]) -> (String, usize) {
    let mut out = String::new();
    let mut slots = Vec::new();
    walk(nodes, &mut out, &mut slots);
    (out, slots.len())
}

fn walk<'a>(nodes: &'a [Node], out: &mut String, slots: &mut Vec<Slot<'a>>) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Field { .. } => {
                let id = slots.len();
                slots.push(Slot::Field(n));
                out.push_str(&marker(id, Kind::Field));
            }
            Node::Present { body, .. } | Node::Absent { body, .. } | Node::Equals { body, .. } => {
                let id = slots.len();
                slots.push(Slot::Block(n));
                out.push_str(&marker(id, Kind::Open));
                walk(body, out, slots);
                out.push_str(&marker(id, Kind::Close));
            }
        }
    }
}

fn marker(id: usize, kind: Kind) -> String {
    let suffix = match kind {
        Kind::Field => "",
        Kind::Open => MARK_OPEN,
        Kind::Close => MARK_CLOSE,
    };
    format!("{MARK_START}{id}{suffix}{MARK_END}")
}

/// This contact's answer for every marker, in the same index order [`marked`] used.
fn fills(nodes: &[Node], c: &Contact<'_>) -> Vec<Fill> {
    let mut slots = Vec::new();
    walk(nodes, &mut String::new(), &mut slots);
    slots
        .iter()
        .map(|slot| match slot {
            Slot::Field(n) => Fill::Value(field_value(n, c)),
            Slot::Block(n) => Fill::Block(branch_taken(n, c)),
        })
        .collect()
}

/// Whether this contact takes a conditional's branch. One definition, so a subject and a body
/// cannot disagree about it.
pub(super) fn branch_taken(n: &Node, c: &Contact<'_>) -> bool {
    match n {
        Node::Present { key, .. } => c.value(key).is_some(),
        Node::Absent { key, .. } => c.value(key).is_none(),
        Node::Equals { key, value, .. } => c.value(key).as_deref() == Some(value.as_str()),
        Node::Text(_) | Node::Field { .. } => false,
    }
}

// ---------------------------------------------------------------------------------------------
// The two public entry points

/// Where each merge tag in `html` lands, in document order. Used at COMPILE time: an `Err` here is
/// a template that is never saved.
///
/// `html` is the output of `mrml`, with [`marked`]'s markers still in it, and `tags` is the count
/// [`marked`] returned. Every index below `tags` must appear in `html` **at least once** — `mrml`
/// legitimately duplicates a marker (an `mj-button` href is emitted twice, once inside the Outlook
/// conditional comment), so the invariant is presence, never equality — **except** for a
/// conditional's two boundary markers, which must appear exactly once each, because suppressing a
/// dropped branch means rewinding between a matching pair and a third copy makes that the wrong
/// range of the message.
///
/// The returned vector is not a reliable index-to-site map when `mrml` duplicated a marker into
/// two different contexts: the first site recorded wins, and a `Dangerous` anywhere wins outright.
/// It is sound for the refusal decision, which is all it is used for. A preview that wants to show
/// an operator *where* each tag landed needs more than this.
pub fn classify(html: &str, tags: usize) -> Result<Vec<Site>, RenderError> {
    classify_send(html, tags, usize::MAX)
}

/// [`classify`] for a send rendition, whose slots from `footer_from` on are the system footer's.
/// Also refuses a footer marker read inside a conditional: dropping that branch would rewind the
/// footer out of the message (review C1).
pub(super) fn classify_send(
    html: &str,
    tags: usize,
    footer_from: usize,
) -> Result<Vec<Site>, RenderError> {
    scan(html, tags, None, footer_from, Links::Off).map(|(sites, ..)| sites)
}

/// Spec §4.3.2: the canonical form of an operator-written `a` or `area` href that is tracked, or
/// `None`. The static allowlist's own verdict, narrowed to `http` and `https`, and never anything
/// under the public URL `base` (without its trailing `/`): the unsubscribe link, an operator's
/// hard-coded `/u/` or confirm link. The caller passes only an href with no merge-tag marker in
/// it, so no contact value can reach the result.
pub(super) fn tracked_url(href: &str, base: &str) -> Option<String> {
    match check_static_url(href) {
        UrlCheck::Allowed(u) if matches!(u.scheme(), "http" | "https") => {
            let u = u.to_string();
            let under_base = u
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with('/'));
            (!under_base).then_some(u)
        }
        _ => None,
    }
}

/// A send rendition's link table (spec §4.3.3): `prior`, the variant's table before this freeze,
/// then each tracked URL of a static `a`/`area` href not already in it, in document order. Walked
/// by the serialiser that renders, in classify mode, so a link inside any conditional branch is
/// seen and the table names exactly what a render can rewrite.
pub(super) fn tracked_links(html: &str, base: &str, prior: &[String]) -> Vec<String> {
    let links = Links::Collect {
        base,
        table: prior.to_vec(),
    };
    // `html` is a document `classify_send` has just accepted, so this cannot fail; were it to, no
    // new link is tracked and every one is sent as the operator wrote it.
    scan(html, 0, None, usize::MAX, links).map_or_else(|_| prior.to_vec(), |(.., table)| table)
}

/// The footer's link for `token`, as the footer writes it: the token percent-encoded as one path
/// segment, exactly as a URL attribute encodes a value there. `(plain text, HTML attribute)`.
pub(super) fn footer_link(prefix: &str, token: &str) -> (String, String) {
    let mut text = prefix.to_owned();
    percent_encode(token, Part::Path.keep(), &mut text);
    let mut html = String::new();
    escape_attr(&text, &mut html);
    (text, html)
}

/// Whether `haystack` holds `link` as a whole link, not as the front of a longer one.
///
/// A plain substring test lets a token that is a prefix of another recipient's token pass on that
/// other link. The character after the match must not be one a token is made of: a letter or
/// digit, `-`, `.`, `_`, `~`, or the `%` that starts an escape. Anything else (a quote, a space,
/// `)`, `<`, the end) ends the link. Wider than a real token's alphabet on purpose: it only has to
/// refuse a continuation, never to accept a link a token could not have made.
pub(super) fn carries_link(haystack: &str, link: &str) -> bool {
    haystack.match_indices(link).any(|(at, _)| {
        !haystack[at + link.len()..]
            .starts_with(|c: char| c.is_ascii_alphanumeric() || "-._~%".contains(c))
    })
}

/// One contact's copy of the already-classified document.
///
/// `html` is the stored output of `mrml` — the same string [`classify`] passed — and `nodes` is the
/// AST it was marked from. Nothing in the types ties those two together, and this function
/// re-scans, so a caller that hands it a DIFFERENT document gets that document's classification
/// rather than none: it will encode correctly for whatever it is given, and silently for the wrong
/// document if it is given the wrong one. Store the HTML `classify` accepted and pass that. A value is substituted and then written out; it is never read back, so
/// a contact whose `first_name` is the text `{{ email }}` gets that text and not somebody's
/// address.
///
/// `links` is a send rendition's `(base, table)`. With `contact.tracking_token` `Some(t)`, a static
/// `a`/`area` href whose [`tracked_url`] is at position `i` of the table is written
/// `{base}/l/{t}/{i}`; every other URL is written exactly as without it.
pub fn substitute(
    html: &str,
    nodes: &[Node],
    contact: &Contact<'_>,
    links: Option<(&str, &[String])>,
) -> Result<String, RenderError> {
    let fills = fills(nodes, contact);
    let links = match (links, contact.tracking_token) {
        (Some((base, table)), Some(token)) => Links::Rewrite { base, table, token },
        _ => Links::Off,
    };
    scan(html, fills.len(), Some(&fills), usize::MAX, links).map(|(_, out, _)| out)
}

/// [`substitute`] for a template that is already the document being sent, with no MJML step.
///
/// **Test-only, deliberately.** Handed an MJML AST this marks the MJML source and classifies a
/// document `mrml` has not rewritten yet, which is exactly the defect the pipeline above exists to
/// prevent — and it would return `Ok`, because it is self-consistent about the wrong document. A
/// doc comment is not enough to stop the next caller reaching for the function named "render", so
/// it is not compiled outside tests. The product's path is `marked` + `classify` at save and
/// [`substitute`] at send.
#[cfg(test)]
fn render_html(nodes: &[Node], contact: &Contact<'_>) -> Result<String, RenderError> {
    let (html, _) = marked(nodes);
    substitute(&html, nodes, contact, None)
}

/// A field tag's value for this contact: the stored value through its filter, or the fallback.
pub(super) fn field_value(node: &Node, c: &Contact<'_>) -> Option<String> {
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
    Mark(usize, Kind),
}

/// Splits a run of text into literal and marker pieces. A `MARK_START` with no digits-then-
/// `MARK_END` after it is not a marker and stays literal text: dropping it would delete six
/// characters from the sent mail with nothing said.
fn segments(s: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut rest = s;
    let mut done = 0;
    while let Some(i) = rest[done..].find(MARK_START).map(|i| done + i) {
        let after = &rest[i + MARK_START.len()..];
        match id_in(after) {
            Some((id, kind, len)) => {
                if i > 0 {
                    out.push(Seg::Lit(&rest[..i]));
                }
                out.push(Seg::Mark(id, kind));
                rest = &after[len + MARK_END.len()..];
                done = 0;
            }
            // Not a marker. Keep it in the literal run and look past it.
            None => done = i + MARK_START.len(),
        }
    }
    if !rest.is_empty() {
        out.push(Seg::Lit(rest));
    }
    out
}

/// The index of every marker in `s`, read exactly as [`classify`] reads one.
pub(super) fn marker_indices(s: &str) -> impl Iterator<Item = usize> + '_ {
    segments(s).into_iter().filter_map(|seg| match seg {
        Seg::Mark(id, _) => Some(id),
        Seg::Lit(_) => None,
    })
}

/// The inside of a marker that has just had its `MARK_START` consumed: the index, which of the
/// three forms it is in, and the byte length of both.
fn id_in(after: &str) -> Option<(usize, Kind, usize)> {
    let len = after.find(MARK_END)?;
    let body = &after[..len];
    let (digits, kind) = if let Some(d) = body.strip_suffix(MARK_OPEN) {
        (d, Kind::Open)
    } else if let Some(d) = body.strip_suffix(MARK_CLOSE) {
        (d, Kind::Close)
    } else {
        (body, Kind::Field)
    };
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
        .map(|id| (id, kind, len))
}

fn position_of(html: &str, id: usize) -> usize {
    [Kind::Field, Kind::Open, Kind::Close]
        .into_iter()
        .find_map(|k| html.find(&marker(id, k)))
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
/// not survive) and in `<style>` raw text (where it never decodes entities at all). `;`, `:`, `(`
/// and `}` are all outside the set, which is what stops a value adding a declaration of its own.
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

/// Percent-encode a value for one component of a URL: everything outside `keep` and the
/// alphanumerics, byte by byte so non-ASCII comes out as correct UTF-8 escapes.
fn percent_encode(s: &str, keep: &str, out: &mut String) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || keep.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

/// Substitute, parse, canonicalise, check the scheme, re-serialise. For the case where the tag IS
/// the whole attribute value.
///
/// Never a blocklist and never a regex for the string `javascript:` — `java\tscript:`,
/// `&#106;avascript:`, `JaVaScript:`, a leading NUL and a leading newline all defeat string
/// matching, and a real URL parser reduces every one of them to the same scheme.
///
/// Userinfo is refused rather than preserved: `http://login.example.com@evil.example/` has an
/// allowed scheme and reads as the operator's domain, which is the whole trick.
fn safe_url(raw: &str) -> String {
    // The URL itself is contact data, so only the reason is ever logged.
    match url::Url::parse(raw) {
        Ok(u) if !u.username().is_empty() || u.password().is_some() => {
            tracing::warn!("merge tag produced a URL carrying userinfo; link disabled");
            DEAD_LINK.to_owned()
        }
        Ok(u) if ALLOWED_SCHEMES.contains(&u.scheme()) => u.to_string(),
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

/// What the allowlist decides about one operator-written URL, before anything is rewritten.
///
/// [`static_url`] is this plus the rewrite. It is public so that the link checker and R7's
/// save-time refusal read the SAME parser and the SAME allowlist: a second URL implementation is
/// the defect, and without a verdict to read, a checker would have had to grow one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlCheck {
    /// Absolute, and the scheme is allowed. Carries the parsed URL so a caller that needs a
    /// component — a `mailto:` address, say — does not reach for a second parser.
    Allowed(url::Url),
    /// No scheme at all, so there is nothing to refuse: left exactly as written. M2c's
    /// unsubscribe links are written this way.
    Relative,
    /// Parsed, but the scheme is outside the frozen allowlist. The renderer emits a dead link.
    Scheme(String),
    /// Neither a URL nor a relative reference. Carries `url`'s own reason.
    Malformed(String),
}

/// The same allowlist over a URL the OPERATOR wrote, with no merge tag in it at all.
///
/// RULING (fix round 1): spec §5.5 wants one renderer for the email and for the hosted
/// view-in-browser page. That page is a real browser on our own origin with no email client's
/// sanitiser in front of it, so a static `javascript:` href an operator put in `mj-raw` is inert
/// in an inbox and stored XSS on our origin the moment the same HTML is served as a page. The
/// operator's own markup is therefore checked too, not only the values substituted into it.
pub fn check_static_url(raw: &str) -> UrlCheck {
    match url::Url::parse(raw) {
        Ok(u) if ALLOWED_SCHEMES.contains(&u.scheme()) => UrlCheck::Allowed(u),
        Ok(u) => UrlCheck::Scheme(u.scheme().to_owned()),
        Err(url::ParseError::RelativeUrlWithoutBase) => UrlCheck::Relative,
        Err(e) => UrlCheck::Malformed(e.to_string()),
    }
}

/// [`check_static_url`] as the renderer needs it: the URL to write out.
///
/// A relative URL is left exactly as written: it cannot name a scheme, so there is nothing to
/// refuse, and M2c's unsubscribe links are written that way.
fn static_url(raw: &str) -> String {
    match check_static_url(raw) {
        UrlCheck::Allowed(u) => u.to_string(),
        // A malformed URL is left as written, exactly as it was before this verdict was split
        // out: it names no scheme to refuse, so it is a dead link in the recipient's client
        // rather than a hazard, and `lint` is what tells the operator about it.
        UrlCheck::Relative | UrlCheck::Malformed(_) => raw.to_owned(),
        UrlCheck::Scheme(scheme) => {
            tracing::warn!(
                scheme = scheme,
                "template holds a URL whose scheme is not allowed; link disabled"
            );
            DEAD_LINK.to_owned()
        }
    }
}

/// Which part of a URL a tag occupies, decided by the literal characters the operator wrote around
/// it. The tag itself is opaque: what matters is the structure the literals create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Scheme,
    Host,
    Path,
    Query,
    Fragment,
}

impl Part {
    /// The characters a value may keep in this component. Everything else is percent-encoded,
    /// which is what stops a value in a query adding `&sig=forged`, or a value in a path climbing
    /// out with `../`.
    fn keep(self) -> &'static str {
        match self {
            Part::Path => "-._~!$&'()*+,;=:@",
            Part::Query | Part::Fragment => "-._~!$'()*+,;:@/?",
            // Refused, never encoded.
            Part::Scheme | Part::Host => "",
        }
    }

    /// A value here decides where the link points, which is the operator handing the destination
    /// to contact data. No encoding fixes that.
    fn is_destination(self) -> bool {
        matches!(self, Part::Scheme | Part::Host)
    }
}

/// Walks the operator's URL template and reports which part each marker falls in, plus the scheme
/// the literal text fixes (`None` for a relative URL).
fn url_parts(segs: &[Seg<'_>]) -> (Vec<(usize, Part)>, Option<String>) {
    let mut parts = Vec::new();
    let mut state = Part::Scheme;
    let mut scheme = String::new();
    let mut first = true;
    for seg in segs {
        let l = match seg {
            // A conditional's markers write nothing, so they take no component: only field
            // markers are positions a value lands in.
            Seg::Mark(id, Kind::Field) => {
                parts.push((*id, state));
                first = false;
                continue;
            }
            Seg::Mark(..) => continue,
            Seg::Lit(l) => *l,
        };
        let b = l.as_bytes();
        let mut i = 0;
        // `//host/x` has no scheme and takes an arbitrary host: the host case wearing a hat.
        if first && state == Part::Scheme && l.starts_with("//") {
            state = Part::Host;
            i = 2;
        }
        first = false;
        while i < b.len() {
            state = match (state, b[i]) {
                (Part::Scheme, b':') => {
                    if b[i + 1..].starts_with(b"//") {
                        i += 2;
                        Part::Host
                    } else {
                        Part::Path
                    }
                }
                (Part::Scheme, b'/') => {
                    scheme.clear();
                    Part::Path
                }
                (Part::Scheme, b'?') => {
                    scheme.clear();
                    Part::Query
                }
                (Part::Scheme, b'#') => {
                    scheme.clear();
                    Part::Fragment
                }
                (Part::Scheme, c) => {
                    scheme.push(c as char);
                    Part::Scheme
                }
                (Part::Host, b'/') => Part::Path,
                (Part::Host, b'?') => Part::Query,
                (Part::Host, b'#') => Part::Fragment,
                (Part::Path, b'?') => Part::Query,
                (Part::Path | Part::Query, b'#') => Part::Fragment,
                (s, _) => s,
            };
            i += 1;
        }
    }
    let scheme = (state != Part::Scheme && !scheme.is_empty()).then(|| scheme.to_ascii_lowercase());
    (parts, scheme)
}

/// The elements whose contents the tokenizer must be told to read as raw text, and how. A tokenizer
/// has no tree builder to say so itself. Shared with `footer`, which must agree with this
/// classifier about where markup is and where it is only text.
pub(super) fn raw_kind(name: &str) -> Option<RawKind> {
    match name {
        "script" => Some(RawKind::ScriptData),
        "style" | "xmp" | "iframe" | "noembed" | "noframes" => Some(RawKind::Rawtext),
        "title" | "textarea" => Some(RawKind::Rcdata),
        _ => None,
    }
}

/// An attribute's site, by element and attribute name.
fn attr_site(elem: &str, name: &str, http_equiv: bool) -> Site {
    // An event handler's value is JavaScript the browser parses AFTER decoding entities, so
    // attribute encoding buys exactly nothing there; `srcdoc` is a whole HTML document; and a
    // `meta refresh` target is a redirect destination written as `0;url=…`, which is a URL with a
    // prefix glued on rather than a URL attribute.
    if name.len() > 2 && name.starts_with("on") {
        return Site::Dangerous;
    }
    if elem == "meta" && name == "content" && http_equiv {
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

/// What the serialiser does with a static `a`/`area` href (spec §4.3.1): the one branch click
/// tracking adds to the function that owns static URLs.
enum Links<'v> {
    Off,
    /// Freezing: each [`tracked_url`] not already in `table` is appended to it.
    Collect {
        base: &'v str,
        table: Vec<String>,
    },
    /// One message: an href whose tracked URL is in `table` points at the click route instead.
    Rewrite {
        base: &'v str,
        table: &'v [String],
        token: &'v str,
    },
}

/// Where in the document's STRUCTURE a marker sits. A conditional is dropped by rewinding the
/// output between its two markers, so both ends must be in the same place: the rewind has to
/// delete a whole region, not the back half of a start tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Outside any tag.
    Text,
    /// The nth attribute VALUE the serialiser has reached. Two markers share a place only when
    /// they are in the same attribute of the same tag.
    Attr(usize),
}

struct St<'v> {
    sites: Vec<Option<Site>>,
    /// `None` when classifying: there is no contact, so nothing is substituted, no branch is
    /// suppressed — every one of them is classified — and `out` is scrap.
    fills: Option<&'v [Fill]>,
    out: String,
    raw: Raw,
    /// A marker index past `MAX_TAGS`, which only a forged marker produces. Refused at the end,
    /// like any other position nobody could classify.
    forged: bool,
    /// How many conditionals are open, and — when a branch is being dropped — the depth it opened
    /// at together with the output length to rewind to.
    depth: usize,
    suppress: Option<(usize, usize)>,
    /// Where the serialiser is, and the open conditionals with where each one started. Together
    /// they are what makes "the rewind deletes a whole region" checkable.
    ///
    /// `attr_seq` only ever increases, and only `attr_value` touches either field — which is what
    /// stops two different attribute values being given the same `Place` and a conditional
    /// straddling them passing for one kept inside a single attribute.
    place: Place,
    attr_seq: usize,
    blocks: Vec<(usize, Place)>,
    /// The first conditional that did not bracket a whole region, or was out of order. Held
    /// rather than returned because the sink cannot return.
    block_err: Option<RenderError>,
    /// Character tokens arrive in chunks split at `&`, `<` and NUL; a run is buffered so a marker
    /// can never be half in one chunk and half in the next.
    text: String,
    /// The first slot of a send rendition's system footer (`usize::MAX` when there is none), and
    /// whether one of its markers was read inside a conditional.
    footer_from: usize,
    footer_in_block: bool,
    links: Links<'v>,
}

impl<'v> St<'v> {
    fn fill(&self, id: usize) -> Option<&'v Fill> {
        self.fills?.get(id)
    }

    fn record(&mut self, id: usize, site: Site) {
        // Every sighting comes through here, decoded, in document order, before the marker's
        // own `emit`: so `depth` is the number of conditionals open around it, character-reference
        // aliases of the operator's own included.
        if id >= self.footer_from && self.depth > 0 {
            self.footer_in_block = true;
        }
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
            if let Seg::Mark(id, _) = seg {
                self.record(id, site);
            }
        }
    }

    /// Writes one marker's replacement. The encoder is chosen by `site`, which came from the
    /// position — never from the value and never from what was recorded earlier.
    fn fail_block(&mut self, e: RenderError) {
        if self.block_err.is_none() {
            self.block_err = Some(e);
        }
    }

    fn emit(&mut self, id: usize, kind: Kind, site: Site) {
        match kind {
            Kind::Field => {
                let Some(Fill::Value(v)) = self.fill(id) else {
                    // Classifying, or an index used in both forms — which `scan`'s count check
                    // refuses, so this arm writes nothing rather than guessing.
                    return;
                };
                let Some(v) = v.as_deref() else { return };
                match site {
                    Site::Text => escape_text(v, &mut self.out),
                    Site::Css => escape_css(v, &mut self.out),
                    Site::Attribute => escape_attr(v, &mut self.out),
                    // A URL is never emitted through here: `url_attr` owns every one of them,
                    // because the encoding depends on the component and not only on the site.
                    Site::Url | Site::Dangerous => {
                        debug_assert!(
                            site == Site::Dangerous,
                            "a Url site must be emitted by url_attr"
                        );
                    }
                }
            }
            Kind::Open => {
                // Recorded whether or not there is a contact: the structural check is part of
                // classification, not of rendering.
                self.blocks.push((id, self.place));
                self.depth += 1;
                if matches!(self.fill(id), Some(Fill::Block(false))) && self.suppress.is_none() {
                    self.suppress = Some((self.depth, self.out.len()));
                }
            }
            Kind::Close => {
                match self.blocks.pop() {
                    Some((open, place)) if open == id => {
                        if place != self.place {
                            self.fail_block(RenderError::BlockStraddlesTag { index: id });
                        }
                    }
                    Some(_) | None => self.fail_block(RenderError::BlockUnbalanced { index: id }),
                }
                if let Some((at, len)) = self.suppress
                    && at == self.depth
                {
                    self.out.truncate(len);
                    self.suppress = None;
                }
                self.depth = self.depth.saturating_sub(1);
            }
        }
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        debug_assert!(
            self.place == Place::Text,
            "character data was flushed while the serialiser was inside an attribute"
        );
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
                Seg::Mark(id, kind) => {
                    self.record(id, site);
                    self.emit(id, kind, site);
                }
            }
        }
    }

    /// `link`: this is the `href` of an `a` or `area` element, which click tracking may rewrite.
    fn attr_value(&mut self, value: &str, site: Site, link: bool) {
        // The place is taken here rather than by the caller on purpose: an attribute value that
        // was serialised without a fresh sequence number would share a `Place` with the previous
        // one, and a conditional straddling the two would pass for one kept inside a single
        // attribute — the exact hole this records a place to close. Doing it here means there is
        // no caller that can forget.
        debug_assert!(
            self.place == Place::Text,
            "attribute values do not nest; a Place was left behind"
        );
        self.attr_seq += 1;
        self.place = Place::Attr(self.attr_seq);
        self.attr_value_inner(value, site, link);
        self.place = Place::Text;
    }

    fn attr_value_inner(&mut self, value: &str, site: Site, link: bool) {
        let segs = segments(value);
        if site == Site::Url {
            if segs.iter().any(|s| matches!(s, Seg::Mark(..))) {
                // A merge tag in a link: never tracked, because its table entry would hold
                // contact data (spec §4.3.2).
                self.url_attr(&segs);
            } else {
                let url = link
                    .then(|| self.tracked(value))
                    .flatten()
                    .unwrap_or_else(|| static_url(value));
                escape_attr(&url, &mut self.out);
            }
            return;
        }
        for seg in &segs {
            match seg {
                Seg::Lit(l) => escape_attr(l, &mut self.out),
                Seg::Mark(id, kind) => {
                    self.record(*id, site);
                    self.emit(*id, *kind, site);
                }
            }
        }
    }

    /// A static `a`/`area` href under [`Links`]: the click route's URL when this message rewrites
    /// it, else `None` and the caller writes [`static_url`] as it always has. The token goes in as
    /// one path segment, as the footer's slots do.
    fn tracked(&mut self, href: &str) -> Option<String> {
        match &mut self.links {
            Links::Off => None,
            Links::Collect { base, table } => {
                if let Some(url) = tracked_url(href, base)
                    && !table.contains(&url)
                {
                    table.push(url);
                }
                None
            }
            Links::Rewrite { base, table, token } => {
                let url = tracked_url(href, base)?;
                let index = table.iter().position(|t| *t == url)?;
                let mut out = format!("{base}/l/");
                percent_encode(token, Part::Path.keep(), &mut out);
                let _ = write!(out, "/{index}");
                Some(out)
            }
        }
    }

    /// A URL attribute that holds at least one tag.
    ///
    /// The whole value being one tag is the ordinary case — a stored link — and gets the scheme
    /// allowlist. Otherwise the operator built the URL and the tag fills in part of it, so the
    /// part decides: scheme or host is refused, and path, query and fragment are percent-encoded
    /// for the component they land in. A value that is only "a valid URL character" is not safe
    /// here; `&`, `=` and `#` are legal in a URL and are exactly how a value injects a parameter.
    fn url_attr(&mut self, segs: &[Seg<'_>]) {
        if let [Seg::Mark(id, Kind::Field)] = segs {
            self.record(*id, Site::Url);
            if let Some(Fill::Value(v)) = self.fill(*id) {
                escape_attr(&safe_url(v.as_deref().unwrap_or_default()), &mut self.out);
            }
            return;
        }
        let (parts, scheme) = url_parts(segs);
        // A tag cannot make `javascript:{{ x }}` safe by being well-encoded.
        let bad_scheme = scheme
            .as_deref()
            .is_some_and(|s| !ALLOWED_SCHEMES.contains(&s));
        let site_of = |p: Part| {
            if bad_scheme || p.is_destination() {
                Site::Dangerous
            } else {
                Site::Url
            }
        };
        if bad_scheme {
            // Every marker, not only the field ones. A conditional has no URL component of its
            // own, so without this a `{{# x }}javascript:{{/ x }}alert(1)` href would hold no
            // field marker for the scheme check to land on and would slip past the static sweep
            // too, because that only runs on an attribute with no markers in it at all.
            for seg in segs {
                if let Seg::Mark(id, _) = seg {
                    self.record(*id, Site::Dangerous);
                }
            }
        }
        let mut next = parts.iter();
        for seg in segs {
            match seg {
                Seg::Lit(l) => escape_attr(l, &mut self.out),
                Seg::Mark(id, Kind::Field) => {
                    let part = next.next().map(|(_, p)| *p).unwrap_or(Part::Scheme);
                    self.record(*id, site_of(part));
                    match self.fill(*id) {
                        Some(Fill::Value(v)) if site_of(part) != Site::Dangerous => {
                            let mut enc = String::new();
                            percent_encode(v.as_deref().unwrap_or_default(), part.keep(), &mut enc);
                            escape_attr(&enc, &mut self.out);
                        }
                        // Refused, so nothing is written; the document is discarded anyway.
                        _ => {}
                    }
                }
                // A conditional kept wholly inside one URL attribute is legitimate; it writes
                // nothing itself, and `url_parts` does not give it a component.
                Seg::Mark(id, kind) => {
                    self.record(*id, Site::Url);
                    self.emit(*id, *kind, Site::Url);
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
            // An end tag's attribute VALUES are not swept here on purpose: the spec drops them, no
            // token reports them, and the fail-closed check in `scan` refuses the template for
            // exactly that reason. A sweep here would be a second line doing the same work, and
            // removing it would leave the suite green — which is how dead safety nets are born.
            self.raw = Raw::Data;
            let _ = write!(self.out, "</{name}>");
            return TokenSinkResult::Continue;
        }

        let http_equiv = tag.attrs.iter().any(|a| &*a.name.local == "http-equiv");
        self.out.push('<');
        self.out.push_str(name);
        for a in &tag.attrs {
            let n: &str = &a.name.local;
            self.out.push(' ');
            self.out.push_str(n);
            // Every attribute is re-serialised with double quotes, whatever the source wrote, so
            // the encoder never has to guess which character ends this value.
            self.out.push_str("=\"");
            let link = n == "href" && matches!(name, "a" | "area");
            self.attr_value(&a.value, attr_site(name, n, http_equiv), link);
            self.out.push('"');
        }
        if tag.self_closing {
            self.out.push_str(" /");
        }
        self.out.push('>');

        let feedback = raw_kind(name);
        self.raw = match feedback {
            None => Raw::Data,
            Some(RawKind::ScriptData) => Raw::Script,
            Some(RawKind::Rcdata) => Raw::Rcdata,
            Some(_) if name == "style" => Raw::Style,
            Some(_) => Raw::Other,
        };
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

/// Classify every marker in `html`, and — when `fills` is given — re-serialise the document with
/// each marker replaced.
///
/// `tags` is how many markers the template emitted. Pre-sizing the table with it is what turns
/// "`mrml` dropped this tag" from a silent `Ok(vec![])` into a refusal.
///
/// `footer_from` is the first slot of a send rendition's system footer, `usize::MAX` for none.
/// The third value is the link table [`Links::Collect`] built; empty for any other `links`.
fn scan<'v>(
    html: &str,
    tags: usize,
    fills: Option<&'v [Fill]>,
    footer_from: usize,
    links: Links<'v>,
) -> Result<(Vec<Site>, String, Vec<String>), RenderError> {
    if tags > MAX_TAGS {
        return Err(RenderError::DangerousContext { position: 0 });
    }
    let mut sites: Vec<Option<Site>> = vec![None; tags];
    // Counted per FORM, not per index: last-occurrence-wins would let a forged field-form alias
    // placed after a conditional's marker reset the flag and skip the check below entirely, and a
    // branch meant to be hidden would then render to every recipient.
    let mut counts: Vec<[usize; 3]> = vec![[0; 3]; tags];
    let bytes = html.as_bytes();
    for (i, _) in html.match_indices(MARK_START) {
        let Some((id, kind, _)) = id_in(&html[i + MARK_START.len()..]) else {
            continue;
        };
        // Bounded BEFORE the resize below: a raw `~nmmt~4294967295~tmmn~` would otherwise ask for
        // four billion entries before anything checked it.
        if id >= MAX_TAGS {
            return Err(RenderError::DangerousContext { position: i });
        }
        if sites.len() <= id {
            sites.resize(id + 1, None);
            counts.resize(id + 1, [0; 3]);
        }
        counts[id][kind as usize] += 1;
        // `<{{ tag }}>`: the tokenizer reads this as TEXT, because a marker is not an ASCII letter
        // and so does not open a tag — but the `<` is already in the template, and a value dropped
        // straight after it is a tag name whatever the tokenizer calls the position.
        if i > 0 && bytes[i - 1] == b'<' {
            sites[id] = Some(Site::Dangerous);
        }
    }

    // A conditional's two markers must each survive exactly once, and the index must not also
    // appear in field form. `mrml` copies markers — measured, on an `mj-button` href — and a
    // copied FIELD marker is harmless because every copy is given the same value. A copied
    // BOUNDARY is not: dropping a branch means rewinding the output between the pair, and a third
    // marker makes that the wrong range. Everything else in this module fails closed; this is one
    // of the two places where not checking would mean a recipient quietly getting the other
    // branch's content. A conditional absent altogether is left to the `TagLost` check below,
    // which says the more useful thing.
    for (id, c) in counts.iter().enumerate() {
        let [field, open, close] = *c;
        if (open > 0 || close > 0) && (open != 1 || close != 1 || field != 0) {
            return Err(RenderError::BlockDuplicated {
                index: id,
                count: field + open + close,
            });
        }
    }

    let tok = Tokenizer::new(
        Sink {
            st: RefCell::new(St {
                sites,
                fills,
                out: String::new(),
                raw: Raw::Data,
                forged: false,
                depth: 0,
                suppress: None,
                place: Place::Text,
                attr_seq: 0,
                blocks: Vec::new(),
                block_err: None,
                text: String::new(),
                footer_from,
                footer_in_block: false,
                links,
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
    // Dropping a branch rewinds the output between a conditional's two markers, so the pair has
    // to bracket a whole region of the document. When it does not — one marker inside an
    // attribute value and the other outside the tag — the rewind lands between `<` and `>`, the
    // serialiser keeps writing, and the rest of the document is parsed as attributes of an
    // unterminated tag. A value encoded for the context it was classified in then arrives in a
    // different one, which is the defect this module exists to prevent.
    if let Some(e) = st.block_err.clone() {
        return Err(e);
    }
    if let Some(&(index, _)) = st.blocks.first() {
        return Err(RenderError::BlockUnbalanced { index });
    }
    if st.footer_in_block {
        return Err(RenderError::FooterInBlock);
    }
    // Fail closed, twice over. A marker the tokenizer never handed back is a position nobody
    // classified; a marker that is not in the document at all is a merge tag `mrml` threw away,
    // and saving that publishes a message with the tag silently missing.
    let mut sites: Vec<Site> = Vec::with_capacity(st.sites.len());
    for (id, site) in st.sites.iter().enumerate() {
        match site {
            Some(Site::Dangerous) | None
                if counts.get(id).is_none_or(|c| c.iter().sum::<usize>() == 0) =>
            {
                return Err(RenderError::TagLost { index: id });
            }
            Some(Site::Dangerous) | None => {
                return Err(RenderError::DangerousContext {
                    position: position_of(html, id),
                });
            }
            Some(s) => sites.push(*s),
        }
    }
    if st.forged {
        return Err(RenderError::DangerousContext { position: 0 });
    }
    let table = match std::mem::replace(&mut st.links, Links::Off) {
        Links::Collect { table, .. } => table,
        Links::Off | Links::Rewrite { .. } => Vec::new(),
    };
    Ok((sites, std::mem::take(&mut st.out), table))
}

// ---------------------------------------------------------------------------------------------
// The plain-text rendition

pub const TEXT_LINK_OPEN: char = '\u{1}';
pub const TEXT_LINK_SEP: char = '\u{2}';
pub const TEXT_LINK_CLOSE: char = '\u{3}';

/// Plain text takes a value exactly as it is. Sibling of `escape_text`.
fn plain_text(s: &str, out: &mut String) {
    out.push_str(s);
}

enum Piece<'a> {
    Text(&'a str),
    Link {
        lead: &'a str,
        url: &'a str,
        tail: &'a str,
    },
}

fn pieces(text: &str) -> Result<Vec<Piece<'_>>, RenderError> {
    let delim = [TEXT_LINK_OPEN, TEXT_LINK_SEP, TEXT_LINK_CLOSE];
    let bad = |at: &str| RenderError::DangerousContext {
        position: text.len() - at.len(),
    };
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(TEXT_LINK_OPEN) {
        let (before, after) = (&rest[..i], &rest[i + TEXT_LINK_OPEN.len_utf8()..]);
        let end = after.find(TEXT_LINK_CLOSE).ok_or_else(|| bad(after))?;
        let mut parts = after[..end].splitn(3, TEXT_LINK_SEP);
        let (Some(lead), Some(url), Some(tail)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad(after));
        };
        if before.contains(delim) || [lead, url, tail].iter().any(|p| p.contains(delim)) {
            return Err(bad(after));
        }
        out.push(Piece::Text(before));
        out.push(Piece::Link { lead, url, tail });
        rest = &after[end + TEXT_LINK_CLOSE.len_utf8()..];
    }
    if rest.contains(delim) {
        return Err(bad(rest));
    }
    out.push(Piece::Text(rest));
    Ok(out)
}

struct TextWalk<'v> {
    fills: Option<&'v [Fill]>,
    blocks: Vec<(usize, usize)>,
    suppress: Option<(usize, usize)>,
    place: usize,
}

impl TextWalk<'_> {
    fn value(&self, id: usize) -> Option<&Option<String>> {
        match self.fills?.get(id)? {
            Fill::Value(v) => Some(v),
            Fill::Block(_) => None,
        }
    }

    fn boundary(&mut self, id: usize, kind: Kind, out: &mut String) -> Result<(), RenderError> {
        match kind {
            Kind::Open => {
                self.blocks.push((id, self.place));
                if matches!(self.fills.and_then(|f| f.get(id)), Some(Fill::Block(false)))
                    && self.suppress.is_none()
                {
                    self.suppress = Some((self.blocks.len(), out.len()));
                }
            }
            Kind::Close => {
                let depth = self.blocks.len();
                match self.blocks.pop() {
                    Some((open, place)) if open == id => {
                        if place != self.place {
                            return Err(RenderError::BlockStraddlesTag { index: id });
                        }
                    }
                    Some(_) | None => return Err(RenderError::BlockUnbalanced { index: id }),
                }
                if let Some((at, len)) = self.suppress
                    && at == depth
                {
                    out.truncate(len);
                    self.suppress = None;
                }
            }
            Kind::Field => {}
        }
        Ok(())
    }

    fn text(&mut self, s: &str, out: &mut String) -> Result<(), RenderError> {
        for seg in segments(s) {
            match seg {
                Seg::Lit(l) => out.push_str(l),
                Seg::Mark(id, Kind::Field) => {
                    if let Some(v) = self.value(id) {
                        plain_text(v.as_deref().unwrap_or_default(), out);
                    }
                }
                Seg::Mark(id, kind) => self.boundary(id, kind, out)?,
            }
        }
        Ok(())
    }

    fn url(&mut self, whole: &str, text: &str, out: &mut String) -> Result<(), RenderError> {
        let segs = segments(whole);
        if !segs.iter().any(|s| matches!(s, Seg::Mark(..))) {
            out.push_str(&static_url(whole));
            return Ok(());
        }
        if let [Seg::Mark(id, Kind::Field)] = segs.as_slice() {
            if let Some(v) = self.value(*id) {
                out.push_str(&safe_url(v.as_deref().unwrap_or_default()));
            }
            return Ok(());
        }
        let (parts, scheme) = url_parts(&segs);
        let dangerous = |id: usize| RenderError::DangerousContext {
            position: position_of(text, id),
        };
        if scheme
            .as_deref()
            .is_some_and(|s| !ALLOWED_SCHEMES.contains(&s))
            && let Some(Seg::Mark(id, _)) = segs.iter().find(|s| matches!(s, Seg::Mark(..)))
        {
            return Err(dangerous(*id));
        }
        let mut next = parts.iter();
        for seg in &segs {
            match seg {
                Seg::Lit(l) => out.push_str(l),
                Seg::Mark(id, Kind::Field) => {
                    let part = next.next().map(|(_, p)| *p).unwrap_or(Part::Scheme);
                    if part.is_destination() {
                        return Err(dangerous(*id));
                    }
                    if let Some(v) = self.value(*id) {
                        percent_encode(v.as_deref().unwrap_or_default(), part.keep(), out);
                    }
                }
                Seg::Mark(id, kind) => self.boundary(*id, *kind, out)?,
            }
        }
        Ok(())
    }
}

fn scan_text(text: &str, tags: usize, fills: Option<&[Fill]>) -> Result<String, RenderError> {
    if tags > MAX_TAGS {
        return Err(RenderError::DangerousContext { position: 0 });
    }
    let mut counts: Vec<[usize; 3]> = vec![[0; 3]; tags];
    for (i, _) in text.match_indices(MARK_START) {
        let Some((id, kind, _)) = id_in(&text[i + MARK_START.len()..]) else {
            continue;
        };
        let Some(c) = counts.get_mut(id) else {
            return Err(RenderError::DangerousContext { position: i });
        };
        c[kind as usize] += 1;
    }
    for (id, [field, open, close]) in counts.iter().copied().enumerate() {
        if field + open + close == 0 {
            return Err(RenderError::TagLost { index: id });
        }
        if (open > 0 || close > 0) && (open != 1 || close != 1 || field != 0) {
            return Err(RenderError::BlockDuplicated {
                index: id,
                count: field + open + close,
            });
        }
    }
    let mut w = TextWalk {
        fills,
        blocks: Vec::new(),
        suppress: None,
        place: 0,
    };
    let mut out = String::new();
    let mut links = 0;
    for piece in pieces(text)? {
        match piece {
            Piece::Text(s) => w.text(s, &mut out)?,
            Piece::Link { lead, url, tail } => {
                if let Some(p) = [lead, tail].into_iter().find(|p| p.contains(MARK_START)) {
                    // `lead` and `tail` are slices of `text`, so the byte offset of the marker
                    // is the slice's distance from the start plus where it sits in the slice.
                    let at = p.as_ptr() as usize - text.as_ptr() as usize;
                    return Err(RenderError::DangerousContext {
                        position: at + p.find(MARK_START).unwrap_or(0),
                    });
                }
                links += 1;
                w.place = links;
                let mut resolved = String::new();
                w.url(url, text, &mut resolved)?;
                w.place = 0;
                if !resolved.is_empty() && resolved != DEAD_LINK {
                    out.push_str(lead);
                    out.push_str(&resolved);
                    out.push_str(tail);
                }
            }
        }
    }
    if let Some(&(index, _)) = w.blocks.first() {
        return Err(RenderError::BlockUnbalanced { index });
    }
    Ok(out)
}

pub fn classify_text(text: &str, tags: usize) -> Result<(), RenderError> {
    scan_text(text, tags, None).map(|_| ())
}

pub fn substitute_text(
    text: &str,
    nodes: &[Node],
    contact: &Contact<'_>,
) -> Result<String, RenderError> {
    let fills = fills(nodes, contact);
    scan_text(text, fills.len(), Some(&fills))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::custom_fields::{Definition, FieldKind};
    use crate::template::tag::{Registry, compile};

    /// `mrml` is run with the default options, which build the CSS inliner with
    /// `load_remote_stylesheets(false)` — see `deny.toml`. Note for whoever ever turns inlining
    /// ON: a `<style>` block becomes a `style` attribute, so a tag classified `Css` in the head
    /// moves into an attribute. Both are `Css` here, so the encoder is the same either way, but
    /// the classification has to be re-run on the inlined output, not on the pre-inline HTML.
    fn render_mjml(mjml: &str) -> String {
        mrml::parse(mjml)
            .expect("mjml parses")
            .element
            .render(&mrml::prelude::render::RenderOptions::default())
            .expect("mjml renders")
    }

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
        let (html, tags) = marked(&nodes(src));
        classify(&html, tags)
    }

    fn contact<'a>(first_name: &'a str, attrs: &'a Value) -> Contact<'a> {
        Contact {
            email: "ada@example.com",
            first_name,
            last_name: "Lovelace",
            timezone: None,
            attributes: attrs,
            unsubscribe_token: "TOK",
            tracking_token: None,
        }
    }

    fn render(src: &str, first_name: &str, attrs: Value) -> Result<String, RenderError> {
        render_html(&nodes(src), &contact(first_name, &attrs))
    }

    fn ok(src: &str, first_name: &str, attrs: Value) -> String {
        render(src, first_name, attrs).expect("the template must render")
    }

    /// `href="…"` with the given value, rendered.
    fn href(template: &str, link: &str) -> String {
        let src = format!("<a href=\"{template}\">x</a>");
        ok(&src, "Ada", json!({ "link": link }))
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
        assert_eq!(
            href("{{ link }}", "https://example.com/a b\"c<d?q=1 2"),
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

    /// An end tag's attributes are dropped by the spec, so a value there would silently vanish.
    /// The name case is caught by the shared name path; the VALUE case has its own line.
    #[test]
    fn a_tag_in_an_end_tag_attribute_fails_to_compile() {
        for src in [
            "<a href=\"x\">y</a {{ first_name }}=\"z\">",
            "<a href=\"x\">y</a alt=\"{{ first_name }}\">",
        ] {
            assert!(
                matches!(sites(src), Err(RenderError::DangerousContext { .. })),
                "{src}: {:?}",
                sites(src)
            );
        }
    }

    #[test]
    fn a_tag_in_srcdoc_fails_to_compile() {
        assert!(matches!(
            sites("<iframe srcdoc=\"<p>{{ first_name }}</p>\"></iframe>"),
            Err(RenderError::DangerousContext { .. })
        ));
        // The same value in an ordinary attribute of the same element is not a refusal.
        assert_eq!(
            sites("<iframe title=\"{{ first_name }}\"></iframe>").unwrap(),
            vec![Site::Attribute]
        );
    }

    /// The fail-closed net. A duplicate attribute is dropped by the tokenizer, so the marker in it
    /// is never handed to any handler — this line is the only thing that catches it.
    #[test]
    fn a_marker_no_token_accounted_for_is_refused() {
        assert!(matches!(
            sites("<a alt=\"y\" alt=\"{{ first_name }}\">x</a>"),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    /// `mrml` dropping a tag is not the same as a template that never had one. Without the count,
    /// this saves clean and the merge tag is gone from every recipient's mail, silently, forever.
    #[test]
    fn a_tag_mrml_dropped_is_refused_rather_than_read_as_no_tags() {
        let src = "<mjml><mj-body><mj-section><mj-column>\
                   <mj-text notanattr=\"{{ colour }}\">hi</mj-text>\
                   </mj-column></mj-section></mj-body></mjml>";
        let (mjml, tags) = marked(&nodes(src));
        assert_eq!(tags, 1);
        let html = render_mjml(&mjml);
        assert!(!html.contains(MARK_START), "mrml must have dropped it");
        assert_eq!(
            classify(&html, tags),
            Err(RenderError::TagLost { index: 0 })
        );
        // And the count is what does it: told to expect none, the same HTML is clean.
        assert_eq!(classify(&html, 0).unwrap(), Vec::<Site>::new());
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
            // Userinfo: an allowed scheme, and the operator's domain in the part a reader trusts.
            "https://app.example.com@evil.example/",
        ] {
            assert_eq!(href("{{ link }}", raw), "<a href=\"#\">x</a>", "{raw:?}");
        }
        // The allowlist is not a wall: the four schemes that are on it still work.
        for raw in [
            "https://example.com/",
            "http://example.com/",
            "mailto:ada@example.com",
            "tel:+15551234567",
        ] {
            assert!(href("{{ link }}", raw).contains(raw), "{raw}");
        }
    }

    /// RULING (fix round 1, F6). A value that decides where a link points is the operator handing
    /// the destination to contact data, and no encoding makes that safe.
    #[test]
    fn a_tag_in_a_url_scheme_or_host_fails_to_compile() {
        for src in [
            "<a href=\"https://{{ link }}.example.com/\">x</a>",
            "<a href=\"{{ link }}://example.com/\">x</a>",
            "<a href=\"ht{{ link }}ps://example.com/\">x</a>",
            // Protocol-relative is the host case wearing a hat: it inherits the scheme and takes
            // an arbitrary host.
            "<a href=\"//{{ link }}/x\">x</a>",
            // And a scheme the operator wrote that is not on the allowlist taints the whole
            // attribute, however well the value is encoded.
            "<a href=\"javascript:{{ link }}\">x</a>",
        ] {
            assert!(
                matches!(sites(src), Err(RenderError::DangerousContext { .. })),
                "{src}: {:?}",
                sites(src)
            );
        }
    }

    /// RULING (fix round 1, F6). Path, query and fragment are percent-encoded for the component
    /// the tag lands in, which keeps the per-recipient links M2c and M2d need working.
    #[test]
    fn a_tag_in_a_url_path_or_query_is_component_encoded() {
        // A path tag cannot climb out of its segment.
        assert_eq!(
            href("https://app.example.com/u/{{ link }}", "../../admin"),
            "<a href=\"https://app.example.com/u/..%2F..%2Fadmin\">x</a>"
        );
        // A query tag cannot add a parameter of its own.
        assert_eq!(
            href(
                "https://app.example.com/?u={{ link }}&sig=abc",
                "1&sig=forged"
            ),
            "<a href=\"https://app.example.com/?u=1%26sig%3Dforged&amp;sig=abc\">x</a>"
        );
        // Nor move the operator's parameters into the fragment, where they are never sent.
        assert_eq!(
            href("https://app.example.com/?u={{ link }}&sig=abc", "1#"),
            "<a href=\"https://app.example.com/?u=1%23&amp;sig=abc\">x</a>"
        );
        // A fragment tag stays in the fragment.
        assert_eq!(
            href("https://app.example.com/#{{ link }}", "a#b"),
            "<a href=\"https://app.example.com/#a%23b\">x</a>"
        );
        // `mailto:` has no authority, so the tag is in the path.
        assert_eq!(
            href("mailto:{{ link }}", "ada@example.com"),
            "<a href=\"mailto:ada@example.com\">x</a>"
        );
        assert_eq!(
            sites("<a href=\"https://app.example.com/u/{{ link }}\">x</a>").unwrap(),
            vec![Site::Url]
        );
    }

    /// RULING (fix round 1, F8). M2c and M2d write per-recipient unsubscribe and tracking links,
    /// and they are relative. Before this they rendered a dead `#` with only a log line.
    #[test]
    fn a_relative_url_works_and_a_protocol_relative_one_does_not() {
        assert_eq!(
            href("/u/{{ link }}", "tok-123"),
            "<a href=\"/u/tok-123\">x</a>"
        );
        assert_eq!(
            href("?token={{ link }}", "a&b=c"),
            "<a href=\"?token=a%26b%3Dc\">x</a>"
        );
        assert!(matches!(
            sites("<a href=\"//{{ link }}/x\">x</a>"),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    /// `xlink:href` is live on the hosted view-in-browser page, and a `meta refresh` target is a
    /// redirect destination with a `0;url=` prefix glued on.
    #[test]
    fn xlink_href_is_a_url_and_meta_refresh_is_refused() {
        assert_eq!(
            sites("<svg><a xlink:href=\"{{ link }}\">x</a></svg>").unwrap(),
            vec![Site::Url]
        );
        assert_eq!(
            ok(
                "<svg><a xlink:href=\"{{ link }}\">x</a></svg>",
                "Ada",
                json!({ "link": "javascript:alert(1)" })
            ),
            "<svg><a xlink:href=\"#\">x</a></svg>"
        );
        assert!(matches!(
            sites("<meta http-equiv=\"refresh\" content=\"0;url={{ link }}\">"),
            Err(RenderError::DangerousContext { .. })
        ));
        // A `content` attribute with no `http-equiv` beside it is an ordinary attribute.
        assert_eq!(
            sites("<meta name=\"description\" content=\"{{ first_name }}\">").unwrap(),
            vec![Site::Attribute]
        );
    }

    /// The encoder standing between a contact value and a CSS context, in both of the places a
    /// CSS context exists.
    #[test]
    fn a_css_position_is_css_escaped() {
        let injection = "red;background:url(https://evil.example/p.png)";
        // In a `style` attribute.
        assert_eq!(
            ok(
                "<p style=\"color:{{ colour }}\">x</p>",
                "Ada",
                json!({ "colour": injection })
            ),
            "<p style=\"color:red\\3b background\\3a url\\28 https\\3a \\2f \\2f evil.example\\2f p.png\\29 \">x</p>"
        );
        // And in a `<style>` element, where entities are never decoded and only this escaping is
        // between the value and the stylesheet.
        assert_eq!(
            ok(
                "<style>.a{color:{{ colour }}}</style>",
                "Ada",
                json!({ "colour": injection })
            ),
            "<style>.a{color:red\\3b background\\3a url\\28 https\\3a \\2f \\2f evil.example\\2f p.png\\29 }</style>"
        );
        // An ordinary colour is left alone, which is the point of the narrow literal set.
        assert_eq!(
            ok(
                "<p style=\"color:{{ colour }}\">x</p>",
                "Ada",
                json!({ "colour": "#ff0000" })
            ),
            "<p style=\"color:#ff0000\">x</p>"
        );
    }

    /// The defect this fix round opened with: `classify` said `Css`, the renderer encoded for the
    /// MJML source's `Attribute`, and a contact field injected CSS declarations into sent mail.
    #[test]
    fn the_renderer_encodes_for_the_document_that_was_classified() {
        let src = "<mjml><mj-body><mj-section><mj-column>\
                   <mj-text color=\"{{ colour }}\">hi</mj-text>\
                   </mj-column></mj-section></mj-body></mjml>";
        let ast = nodes(src);
        let (mjml, tags) = marked(&ast);
        let html = render_mjml(&mjml);
        assert_eq!(classify(&html, tags).unwrap(), vec![Site::Css]);

        let attrs = json!({ "colour": "red;background:url(https://evil.example/p.png)" });
        let out = substitute(&html, &ast, &contact("Ada", &attrs), None).unwrap();
        assert!(
            !out.contains("background:url("),
            "a contact value injected a CSS declaration: {out}"
        );
        assert!(out.contains("color:red\\3b "), "{out}");
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
        // Nested, and the inner branch of a dropped outer one is dropped with it.
        let nested = "{{# link }}A{{# bio }}B{{/ bio }}C{{/ link }}D";
        assert_eq!(ok(nested, "x", json!({ "link": "y", "bio": "z" })), "ABCD");
        assert_eq!(ok(nested, "x", json!({ "link": "y" })), "ACD");
        assert_eq!(ok(nested, "x", json!({ "bio": "z" })), "D");
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
        let (mjml, tags) = marked(&nodes(src));
        // In the MJML source the tag sits in an ordinary attribute of a custom element.
        assert_eq!(classify(&mjml, tags).unwrap(), vec![Site::Attribute]);
        // `mrml` moves it into a `style` attribute on a real `div`, which is a CSS position — so
        // classifying the source would have encoded it for the wrong context entirely.
        let html = render_mjml(&mjml);
        assert!(html.contains(MARK_START), "the marker must survive mrml");
        assert_eq!(classify(&html, tags).unwrap(), vec![Site::Css]);
    }

    /// The whole compile path on a real document: `mrml` wraps body content in `<!--[if mso]>`
    /// blocks and in a `<style>` head, so a tag in ordinary text has to come back `Text` and not
    /// get caught by the comment or raw-text rules. `mrml` also DUPLICATES the button href into
    /// the Outlook fallback, which is why the invariant is "present at least once".
    #[test]
    fn an_ordinary_merge_tag_survives_a_real_mrml_document() {
        let src = "<mjml><mj-body><mj-section><mj-column>\
                   <mj-text>Hi {{ first_name }}</mj-text>\
                   <mj-button href=\"{{ link }}\">Go</mj-button>\
                   </mj-column></mj-section></mj-body></mjml>";
        let (mjml, tags) = marked(&nodes(src));
        let html = render_mjml(&mjml);
        assert_eq!(classify(&html, tags).unwrap(), vec![Site::Text, Site::Url]);
    }

    #[test]
    fn unicode_survives_text_and_punycodes_an_idn_host() {
        // U+202E right-to-left override, U+200D zero-width joiner, U+0301 combining acute.
        let name = "a\u{202E}b\u{200D}c\u{0301}d";
        assert_eq!(
            ok("<p>{{ first_name }}</p>", name, json!({})),
            format!("<p>{name}</p>")
        );
        assert_eq!(
            href("{{ link }}", "https://exämple.com/ü?q=é"),
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

    /// A `MARK_START` that is not a marker is literal text an operator wrote. Dropping it deletes
    /// six characters from the sent mail with nothing said.
    #[test]
    fn an_unmatched_marker_start_stays_literal_text() {
        let src = format!("<p>a{MARK_START}b {{{{ first_name }}}}</p>");
        assert_eq!(
            ok(&src, "Ada", json!({})),
            format!("<p>a{MARK_START}b Ada</p>")
        );
    }

    /// A marker index is a number in HTML that came from operator input, so it has to be bounded
    /// before it sizes anything — on the raw path AND on the entity-encoded one, which reach the
    /// bound by different routes.
    #[test]
    fn a_forged_marker_index_is_refused_not_allocated_for() {
        assert_eq!(MAX_TAGS, 10_000);
        // Raw: caught by the prescan, before the resize it guards.
        let raw = format!("<p>{MARK_START}4294967295{MARK_END}{MARK_START}0{MARK_END}</p>");
        assert!(matches!(
            classify(&raw, 1),
            Err(RenderError::DangerousContext { .. })
        ));
        // The count itself is caller-supplied and sizes a vector before the prescan runs, so it
        // is bounded too.
        assert!(matches!(
            classify("", MAX_TAGS + 1),
            Err(RenderError::DangerousContext { .. })
        ));
        // `~` written as `&#126;`: the raw prescan sees character references, the tokenizer sees a
        // marker with an index that would size a vector.
        let encoded =
            format!("<p>&#126;nmmt&#126;4294967295&#126;tmmn&#126;{MARK_START}0{MARK_END}</p>");
        assert!(matches!(
            classify(&encoded, 1),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    /// RULING (fix round 1). The same renderer serves the hosted view-in-browser page, where a
    /// `javascript:` href is stored XSS on our own origin and no inbox sanitiser stands in the
    /// way — so the operator's own markup goes through the allowlist too, merge tag or not.
    #[test]
    fn a_static_url_the_operator_wrote_goes_through_the_allowlist_too() {
        let plain = |src: &str| ok(src, "Ada", json!({}));
        for raw in [
            "javascript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html;base64,PHNjcmlwdD4=",
            "vbscript:msgbox(1)",
        ] {
            assert_eq!(
                plain(&format!("<a href=\"{raw}\">x</a>")),
                "<a href=\"#\">x</a>",
                "{raw:?}"
            );
        }
        // A relative URL cannot name a scheme, so it is left exactly as written. M2c and M2d
        // write unsubscribe and tracking links this way.
        for raw in ["/u/abc", "#", "?t=1", "https://example.com/"] {
            let src = format!("<a href=\"{raw}\">x</a>");
            assert_eq!(plain(&src), src, "{raw:?}");
        }
    }

    /// Everything else in this module fails closed. An unevenly duplicated conditional boundary
    /// would not: it would suppress the wrong range of a sent message and say nothing, so a
    /// recipient sees the other branch's content. `mrml 6.0.1` only duplicates attribute values
    /// today, which is an observation about one version and not a guarantee.
    #[test]
    fn a_duplicated_conditional_boundary_is_refused() {
        // A conditional is one slot writing two markers that share its index, so the pair names
        // itself and `classify` can check it without being handed a map.
        let (html, tags) = marked(&nodes("{{# link }}hi{{/ link }}"));
        assert_eq!(tags, 1);
        assert_eq!(classify(&html, tags).unwrap(), vec![Site::Text]);
        for kind in [Kind::Open, Kind::Close] {
            let m = marker(0, kind);
            let doubled = html.replacen(&m, &format!("{m}{m}"), 1);
            assert_eq!(
                classify(&doubled, tags),
                Err(RenderError::BlockDuplicated { index: 0, count: 3 }),
                "{kind:?}"
            );
        }
        // A duplicated FIELD marker stays legal: every copy is given the same value, which is
        // what `mrml` does to an `mj-button` href and what a real document relies on.
        let (html, tags) = marked(&nodes("<p>{{ first_name }}</p>"));
        let m = marker(0, Kind::Field);
        let doubled = html.replacen(&m, &format!("{m}{m}"), 1);
        assert_eq!(classify(&doubled, tags).unwrap(), vec![Site::Text]);
        assert_eq!(
            substitute(
                &doubled,
                &nodes("<p>{{ first_name }}</p>"),
                &contact("Ada", &json!({})),
                None
            )
            .unwrap(),
            "<p>AdaAda</p>"
        );
    }

    /// Dropping a branch rewinds the output between a conditional's two markers. If they do not
    /// bracket a whole region the rewind lands between `<` and `>`, the serialiser keeps writing,
    /// and the rest of the document is parsed as attributes of an unterminated tag — so a value
    /// encoded as TEXT, where `escape_text` leaves `"` alone, arrives in attribute position. That
    /// is this module's own defect reached through the conditional, and it survives real `mrml`.
    #[test]
    fn a_conditional_that_straddles_a_tag_is_refused() {
        for src in [
            "<a href=\"https://x.example/{{# link }}\">hi{{/ link }}</a><p>{{ first_name }}</p>",
            "<a title=\"t{{# link }}\">x</a><img alt=\"a{{/ link }}\">",
            "<p title=\"t{{# link }}\">body{{/ link }}</p>",
        ] {
            assert!(
                matches!(sites(src), Err(RenderError::BlockStraddlesTag { index: 0 })),
                "{src}: {:?}",
                sites(src)
            );
        }
        // Nor can a conditional smuggle a disallowed scheme past the static sweep by wrapping
        // it: the sweep only runs on an attribute with no markers at all, and a conditional has
        // no URL component for the scheme check to land on.
        assert!(matches!(
            sites("<a href=\"{{# link }}javascript:{{/ link }}alert(1)\">x</a>"),
            Err(RenderError::DangerousContext { .. })
        ));
        // Kept wholly inside ONE attribute value it is legitimate and must keep working.
        let src = "<a href=\"https://app.example.com/go{{# link }}?u=1{{/ link }}\">x</a>";
        assert_eq!(sites(src).unwrap(), vec![Site::Url]);
        assert_eq!(
            ok(src, "Ada", json!({ "link": "y" })),
            "<a href=\"https://app.example.com/go?u=1\">x</a>"
        );
        assert_eq!(
            ok(src, "Ada", json!({})),
            "<a href=\"https://app.example.com/go\">x</a>"
        );
    }

    /// The check that a conditional's markers survive exactly once is keyed on the index, so an
    /// index that ALSO appears in field form would hide the pair from it. Placed after the real
    /// markers that used to reset a last-occurrence-wins flag, and the hidden branch then rendered
    /// to every recipient.
    #[test]
    fn a_forged_field_alias_cannot_hide_a_conditional() {
        let ast = nodes("{{# link }}SECRET{{/ link }}TAIL");
        let (html, tags) = marked(&ast);
        for forged in [
            format!("{html}{}", marker(0, Kind::Field)),
            format!("{}{html}", marker(0, Kind::Field)),
        ] {
            assert!(
                matches!(
                    classify(&forged, tags),
                    Err(RenderError::BlockDuplicated { index: 0, .. })
                ),
                "{:?}",
                classify(&forged, tags)
            );
            assert!(substitute(&forged, &ast, &contact("Ada", &json!({})), None).is_err());
        }
    }

    /// RULING (fix round 3). `mrml 6.0.1` does not reorder markers — an observation about one
    /// version, and this module has been bitten three times by exactly that kind of observation.
    /// Counting without pairing lets a reordered pair through with counts of one and one.
    #[test]
    fn a_reordered_or_crossed_conditional_pair_is_refused() {
        let (_, tags) = marked(&nodes("{{# link }}body{{/ link }}"));
        let swapped = format!("{}body{}", marker(0, Kind::Close), marker(0, Kind::Open));
        assert_eq!(
            classify(&swapped, tags),
            Err(RenderError::BlockUnbalanced { index: 0 })
        );
        // Crossed rather than nested: every count is one and every marker is present.
        let (_, tags) = marked(&nodes("{{# link }}a{{/ link }}{{# bio }}b{{/ bio }}"));
        assert_eq!(tags, 2);
        let crossed = format!(
            "{}{}{}{}",
            marker(0, Kind::Open),
            marker(1, Kind::Open),
            marker(0, Kind::Close),
            marker(1, Kind::Close)
        );
        assert_eq!(
            classify(&crossed, tags),
            Err(RenderError::BlockUnbalanced { index: 0 })
        );
        // An open whose close is somewhere no token reports it — here a comment. The document is
        // refused either way, because the close is a dangerous position too; the leftover check
        // is what makes the refusal say "your conditional is unbalanced" instead of pointing at
        // the comment.
        let (_, tags) = marked(&nodes("{{# link }}body{{/ link }}"));
        let orphan = format!(
            "{}body<!--{}-->",
            marker(0, Kind::Open),
            marker(0, Kind::Close)
        );
        assert_eq!(
            classify(&orphan, tags),
            Err(RenderError::BlockUnbalanced { index: 0 })
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

    /// Spec §4.3.2's predicate, one href at a time: absolute `http`/`https` through the static
    /// allowlist, canonicalised, and never under the public URL.
    #[test]
    fn tracked_url_takes_http_and_https_off_the_public_url_and_nothing_else() {
        let base = "https://mail.example.test";
        for (href, want) in [
            ("https://a.test/x", Some("https://a.test/x")),
            ("HTTP://B.TEST", Some("http://b.test/")),
            (
                "https://mail.example.test.evil.test/",
                Some("https://mail.example.test.evil.test/"),
            ),
            ("https://mail.example.test/u/abc", None),
            ("HTTPS://MAIL.EXAMPLE.TEST:443/l/x/0", None),
            ("https://mail.example.test", None),
            ("mailto:a@b.test", None),
            ("tel:+100", None),
            ("javascript:alert(1)", None),
            ("#top", None),
            ("/relative", None),
            ("//evil.test/", None),
            ("https://", None),
        ] {
            assert_eq!(tracked_url(href, base).as_deref(), want, "{href}");
        }
    }
}

/// The plain-text rendition: one evaluator, a verbatim encoder, and links that are encoded as URLs.
#[cfg(test)]
mod text_tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::template::tag::{Registry, compile};

    const O: char = TEXT_LINK_OPEN;
    const S: char = TEXT_LINK_SEP;
    const C: char = TEXT_LINK_CLOSE;

    fn nodes(src: &str) -> Vec<Node> {
        compile(src, &Registry::from_definitions(&[])).expect("the template must compile")
    }

    fn contact<'a>(first_name: &'a str, attrs: &'a Value) -> Contact<'a> {
        Contact {
            email: "ada@example.com",
            first_name,
            last_name: "Lovelace",
            timezone: None,
            attributes: attrs,
            unsubscribe_token: "TOK",
            tracking_token: None,
        }
    }

    /// Mark `src`, classify the result as the compile step does, then fill it.
    fn render(src: &str, first_name: &str) -> Result<String, RenderError> {
        let n = nodes(src);
        let (text, tags) = marked(&n);
        classify_text(&text, tags)?;
        substitute_text(&text, &n, &contact(first_name, &json!({})))
    }

    fn m(id: usize, suffix: &str) -> String {
        format!("{MARK_START}{id}{suffix}{MARK_END}")
    }

    #[test]
    fn a_value_is_written_as_the_characters_it_holds() {
        assert_eq!(
            render("Hi {{ first_name }}!", "A&B <i> \"q\" it's &amp;").unwrap(),
            "Hi A&B <i> \"q\" it's &amp;!"
        );
    }

    #[test]
    fn a_value_is_never_read_back_as_a_tag() {
        assert_eq!(
            render("{{ first_name }}", "{{ email }}").unwrap(),
            "{{ email }}"
        );
    }

    #[test]
    fn a_conditional_keeps_or_drops_what_is_between_its_markers() {
        let src = "a{{# first_name }}b{{# email }}c{{/ email }}d{{/ first_name }}e{{^ first_name }}f{{/ first_name }}";
        assert_eq!(render(src, "Ada").unwrap(), "abcde");
        assert_eq!(render(src, "").unwrap(), "aef");
    }

    #[test]
    fn a_link_is_its_lead_url_and_tail_when_the_url_is_usable() {
        let src = format!("x{O} ({S}https://a.example/u/{{{{ first_name }}}}?q=1{S}){C}y");
        // A tag in a path is percent-encoded for the path, like an `href` in the HTML part.
        assert_eq!(
            render(&src, "A B&C").unwrap(),
            "x (https://a.example/u/A%20B&C?q=1)y"
        );
    }

    #[test]
    fn a_link_with_no_usable_url_vanishes_with_its_lead_and_tail() {
        let whole = format!("x{O}: {S}{{{{ first_name }}}}{S}{C}y");
        for bad in [
            "javascript:alert(1)",
            "not a url",
            "https://u:p@evil.example/",
            "",
        ] {
            assert_eq!(render(&whole, bad).unwrap(), "xy", "{bad:?}");
        }
        assert_eq!(
            render(&whole, "https://a.example/ok").unwrap(),
            "x: https://a.example/oky"
        );
        let literal = format!("x{O}: {S}javascript:alert(1){S}{C}y");
        assert_eq!(render(&literal, "Ada").unwrap(), "xy");
        let plain = format!("x{O}: {S}https://a.example/p{S}{C}y");
        assert_eq!(render(&plain, "Ada").unwrap(), "x: https://a.example/py");
    }

    #[test]
    fn a_conditional_inside_a_link_is_kept_or_dropped_inside_it() {
        let src =
            format!("{O}{S}https://a.example/{{{{# first_name }}}}x{{{{/ first_name }}}}{S}{C}");
        assert_eq!(render(&src, "Ada").unwrap(), "https://a.example/x");
        assert_eq!(render(&src, "").unwrap(), "https://a.example/");
    }

    #[test]
    fn a_tag_that_decides_where_a_link_points_is_refused() {
        for url in [
            "https://{{ first_name }}.example/",
            "{{ first_name }}://a.example/",
            "javascript:{{ first_name }}",
            "javascript:{{# first_name }}x{{/ first_name }}",
        ] {
            let src = format!("{O}{S}{url}{S}{C}");
            assert!(
                matches!(
                    render(&src, "Ada"),
                    Err(RenderError::DangerousContext { .. })
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn a_marker_that_is_not_in_the_template_is_a_lost_tag() {
        assert_eq!(
            classify_text("nothing here", 1),
            Err(RenderError::TagLost { index: 0 })
        );
    }

    #[test]
    fn a_marker_beyond_the_count_is_forged() {
        assert!(matches!(
            classify_text(&m(3, ""), 1),
            Err(RenderError::DangerousContext { .. })
        ));
    }

    #[test]
    fn conditional_boundaries_must_be_one_open_one_close_in_order() {
        let ok = format!("{}x{}", m(0, "o"), m(0, "c"));
        assert_eq!(classify_text(&ok, 1), Ok(()));
        // Duplicated, and in field form as well.
        let twice = format!("{}x{}{}", m(0, "o"), m(0, "c"), m(0, "c"));
        assert!(matches!(
            classify_text(&twice, 1),
            Err(RenderError::BlockDuplicated { index: 0, .. })
        ));
        let aliased = format!("{}x{}{}", m(0, "o"), m(0, "c"), m(0, ""));
        assert!(matches!(
            classify_text(&aliased, 1),
            Err(RenderError::BlockDuplicated { index: 0, .. })
        ));
        // Open with no close, close with no open, and crossed.
        let open_only = format!("{}x", m(0, "o"));
        assert!(matches!(
            classify_text(&open_only, 1),
            Err(RenderError::BlockDuplicated { index: 0, .. })
        ));
        let crossed = format!("{}{}{}{}", m(0, "o"), m(1, "o"), m(0, "c"), m(1, "c"));
        assert_eq!(
            classify_text(&crossed, 2),
            Err(RenderError::BlockUnbalanced { index: 0 })
        );
        let reversed = format!("{}{}", m(0, "c"), m(0, "o"));
        assert_eq!(
            classify_text(&reversed, 1),
            Err(RenderError::BlockUnbalanced { index: 0 })
        );
    }

    #[test]
    fn a_conditional_may_not_start_outside_a_link_and_end_inside_it() {
        let text = format!("{}{O}{S}https://a.example/{}{S}{C}", m(0, "o"), m(0, "c"));
        assert_eq!(
            classify_text(&text, 1),
            Err(RenderError::BlockStraddlesTag { index: 0 })
        );
        let other_way = format!("{O}{S}https://a.example/{}{S}{C}{}", m(0, "o"), m(0, "c"));
        assert_eq!(
            classify_text(&other_way, 1),
            Err(RenderError::BlockStraddlesTag { index: 0 })
        );
    }

    #[test]
    fn a_malformed_link_span_is_refused() {
        for text in [
            format!("{O}{S}https://a.example/"),
            format!("{O}only-one-part{C}"),
            format!("{O}{S}u{S}{C}{S}"),
            format!("stray {C} close"),
            format!("{O}{S}a{O}{S}b{S}{C}{S}{C}"),
        ] {
            assert!(
                matches!(
                    classify_text(&text, 0),
                    Err(RenderError::DangerousContext { .. })
                ),
                "{text:?}"
            );
        }
    }
}
