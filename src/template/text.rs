// SPDX-License-Identifier: AGPL-3.0-only

//! The plain-text part of a message, generated from the BLOCK TREE and never from the rendered HTML:
//! the tree knows a heading is a heading and a button is a call to action, where the HTML knows
//! only tables.
//!
//! Plain text receives CHARACTERS, never entities. listmonk runs its alternative body through
//! `html/template` and delivers `&amp;` and `&#39;` to people reading plain text;
//! `the_output_contains_no_html_entities` is the test that stops this product repeating it.
//!
//! # The pipeline mirrors the HTML one
//!
//! ```text
//! save:  blocks + brand --text source--> tag::compile --> AST --render::marked--> marked text
//!        --render::classify_text--> refuse, or store { nodes, text }
//! send:  render::substitute_text(stored text, stored nodes, contact) --tidy--> the message
//! ```
//!
//! The same evaluator fills both parts (`render.rs`'s `fills`: values, filters, fallbacks,
//! conditionals) from stored ASTs, and only the encoder differs: `render::plain_text` writes a value
//! as it is, a link's URL gets the encoding an `href` gets. There is no registry at send, so a field
//! archived after save cannot change what this part renders.
//!
//! **Assumption: the two parts cannot disagree only while [`compile_text`] and
//! `mjml::compile_blocks` run at the same instant against the same `Registry`.** They are two
//! compilations from two sources, each with its own AST. Compile both in one function and store
//! both in one transaction.
//!
//! # What each block looks like
//!
//! Blocks are separated by one blank line. Bold and italic are dropped: `*x*` in plain text is
//! markup of our own invention.
//!
//! * Heading: its own line. Levels are not told apart; underlining needs the substituted width.
//! * Button: `Label: URL`. There is nothing to click, so the URL has to be on the page.
//! * Link run: `text (URL)`, or the bare text when it already is the URL.
//! * Image: `[alt]`, nothing at all when the alt is empty.
//! * Divider: a rule of 40 hyphens. Spacer: nothing; the blank line between blocks is the space.
//! * Columns: read in order, column after column.
//! * The brand kit's footer text and postal address follow, because CAN-SPAM and CASL require the
//!   address in the text alternative too.
//!
//! A URL the HTML part would disable (`javascript:`, a bad scheme, an unparsable value from a tag)
//! is left out here too, together with its `: ` or brackets: the label stays, the link does not.
//!
//! # Reserved characters
//!
//! Operator TEXT (runs, labels, alt, hrefs, the text beside a tag in a `Run::Tag`, the footer) may not
//! contain a typed `~nmmt~`, the brace stand-in U+E000 or a link delimiter U+0001 to U+0003: each is
//! refused as `CompileError::ForgedMarker`, never deleted or rewritten.
//!
//! **A tag's own quoted literals are exempt**: a fallback (`{{ x | "..." }}`) or an equality value
//! (`{{# x = "..." }}`) is passed through unchecked, and may hold those characters. The reason is not
//! that the check was skipped. The brace stand-in was a bug because the operator typed `{` and we
//! stored our own character instead, which is storing something they did not write. A quoted literal is
//! the operator writing that character on purpose, so storing it is correct, and it cannot do
//! structural harm: a fill is written after `scan_text` and is never scanned again.

use std::cell::RefCell;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, EndTag, StartTag, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use super::block::{Block, Run};
use super::mjml::{BrandKit, CompileError};
use super::render::{
    self, Contact, MARK_START, RenderError, TEXT_LINK_CLOSE, TEXT_LINK_OPEN, TEXT_LINK_SEP,
};
use super::tag::{self, Node, Registry};

const RULE: &str = "----------------------------------------";

/// Stands in for `{` in operator text so the whole template compiles as one tag source and typed
/// `{{ x }}` stays literal (contract section 13). Put back after the markers are written.
const BRACE: char = '\u{E000}';

/// A template that has been through the whole save pipeline. `text` is the marked plain text, already
/// classified; `nodes` is the AST it was marked from. Store both with `render::MARKER_FORMAT_VERSION`
/// and pass them to [`render_text`].
///
/// `Serialize`/`Deserialize` because R7 stores it in `templates.compiled` beside the marked HTML:
/// both renditions are written in one transaction and filled from one AST, which is what stops the
/// two halves of a message disagreeing about the recipient.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextTemplate {
    pub nodes: Vec<Node>,
    pub text: String,
}

/// Operator text into the template source. Plain text has no entities, so the only change is the one
/// that keeps a typed `{` from being read as a tag (see [`BRACE`]).
///
/// The characters this module uses as its own stand-in and delimiters (`BRACE`, and the three link
/// delimiters) are RESERVED: operator text may not contain them. `lit` cannot fail, so it writes the
/// one thing `compile_text` already refuses, the marker text, and the refusal is the same
/// `ForgedMarker` as for a typed marker. Dropping or rewriting them instead would be silent data
/// loss.
fn lit(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '{' => out.push(BRACE),
            BRACE | TEXT_LINK_OPEN | TEXT_LINK_SEP | TEXT_LINK_CLOSE => out.push_str(MARK_START),
            c => out.push(c),
        }
    }
}

/// `s` with merge tags let through and everything else a literal. The rule is `mjml::emit`'s, byte
/// for byte: a `{{`, then the first `}}` after it.
fn emit(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + len + 2;
        lit(&rest[..start], out);
        out.push_str(&rest[start..end]);
        rest = &rest[end..];
    }
    lit(rest, out);
}

/// A link: `lead url tail` when the URL is usable at send, nothing when it is not.
fn link(href: &str, lead: &str, tail: &str, out: &mut String) {
    out.push(TEXT_LINK_OPEN);
    out.push_str(lead);
    out.push(TEXT_LINK_SEP);
    emit(href, out);
    out.push(TEXT_LINK_SEP);
    out.push_str(tail);
    out.push(TEXT_LINK_CLOSE);
}

fn runs(rs: &[Run], out: &mut String) {
    for r in rs {
        match r {
            Run::Text { text, .. } => lit(text, out),
            Run::Tag { source } => emit(source, out),
            Run::Link { href, text, .. } => {
                let (t, h) = (text.trim(), href.trim());
                if t.is_empty() {
                    link(href, "", "", out);
                } else if t == h || t == h.trim_start_matches("mailto:") {
                    // The text already says where it goes; the address would only repeat it.
                    lit(text, out);
                } else {
                    lit(text, out);
                    link(href, " (", ")", out);
                }
            }
        }
    }
}

fn block(b: &Block, out: &mut String) {
    match b {
        Block::Text { runs: rs } | Block::Heading { runs: rs, .. } => runs(rs, out),
        Block::Image { alt, .. } => {
            if !alt.trim().is_empty() {
                out.push('[');
                lit(alt, out);
                out.push(']');
            }
        }
        Block::Button { label, href } => {
            if !label.trim().is_empty() {
                lit(label, out);
            }
            link(
                href,
                if label.trim().is_empty() { "" } else { ": " },
                "",
                out,
            );
        }
        Block::Divider {} => out.push_str(RULE),
        Block::Spacer { .. } => {}
        Block::Columns { children } => {
            for child in children.iter().flatten() {
                block(child, out);
                out.push_str("\n\n");
            }
        }
    }
}

/// The template source for `blocks` under `brand`: the tree's text with merge tags let through
/// where the HTML lets them through (a `Run::Tag`, and a URL field) and nowhere else.
fn source(blocks: &[Block], brand: &BrandKit) -> String {
    let mut out = String::new();
    for b in blocks {
        block(b, &mut out);
        out.push_str("\n\n");
    }
    let footer: Vec<&str> = [&brand.footer_text, &brand.footer_address]
        .into_iter()
        .filter_map(|f| f.as_deref())
        .filter(|f| !f.trim().is_empty())
        .collect();
    for f in footer {
        lit(f, &mut out);
        out.push('\n');
    }
    out
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

fn restore_braces(nodes: &mut [Node]) {
    for n in nodes {
        match n {
            Node::Text(t) => *t = t.replace(BRACE, "{"),
            Node::Field { .. } => {}
            Node::Present { body, .. } | Node::Absent { body, .. } | Node::Equals { body, .. } => {
                restore_braces(body)
            }
        }
    }
}

/// Save a `blocks` template's plain-text part. Run it with `mjml::compile_blocks`, in the same
/// call and against the same `registry`: that is what makes the two parts agree.
pub fn compile_text(
    blocks: &[Block],
    brand: &BrandKit,
    registry: &Registry,
) -> Result<TextTemplate, CompileError> {
    // An invalid kit is an error, never a different email than the one configured.
    brand.validate()?;
    let mut nodes = tag::compile(&source(blocks, brand), registry).map_err(CompileError::Tag)?;
    if has_literal_marker(&nodes) {
        return Err(CompileError::ForgedMarker);
    }
    // The braces are put back in the AST, and ONLY there: the stored text is then written from the
    // restored AST by `marked`, so the two cannot disagree about it and neither can be skipped
    // without the other showing.
    restore_braces(&mut nodes);
    let (text, tags) = render::marked(&nodes);
    render::classify_text(&text, tags).map_err(CompileError::Render)?;
    Ok(TextTemplate { nodes, text })
}

/// One contact's plain-text part from the stored template. No registry: the AST is the one stored
/// at save, as for the HTML part.
pub fn render_text(t: &TextTemplate, contact: &Contact<'_>) -> Result<String, RenderError> {
    render::substitute_text(&t.text, &t.nodes, contact).map(|s| tidy(&s))
}

/// Trailing spaces off every line, no more than one blank line in a row, nothing at either end.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = false;
    for line in s.lines().map(str::trim_end) {
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if blank {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        blank = false;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Raw mode

/// The plain-text part of an HTML document: the raw-mode fallback, where there is no block tree.
///
/// `table`, `tr` and `td` are BLOCK here. Marketing HTML is almost entirely tables, and collapsing
/// them as inline text gives one unreadable line. The input is tokenized with `html5ever`, the
/// parser the rest of the template code already agrees with a browser on, so entities arrive
/// decoded, `<script>` and `<style>` are raw text, and a conditional comment is a comment.
///
/// shortcut: no `<pre>` handling (its whitespace collapses like any other) and every list is
/// bulleted `- `, numbered or not. Add them if raw templates turn out to need either.
pub fn text_from_html(html: &str) -> String {
    let tok = Tokenizer::new(
        Sink {
            st: RefCell::new(Flow::default()),
        },
        TokenizerOpts::default(),
    );
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tok.feed(&input);
    tok.end();
    let out = std::mem::take(&mut tok.sink.st.borrow_mut().out);
    tidy(&out)
}

#[derive(Default)]
struct Flow {
    out: String,
    /// Newlines owed before the next visible character, so that two boundaries in a row make one
    /// break and a boundary at the very end makes none.
    owed: u8,
    space: bool,
    prefix: &'static str,
    /// The element whose contents are not content (`script`, `style`, `title`, `head`).
    skip: Option<String>,
    link: Option<(String, String)>,
}

impl Flow {
    fn breaks(&mut self, n: u8) {
        self.owed = self.owed.max(n);
    }

    fn text(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_whitespace() {
                self.space = true;
                continue;
            }
            if !self.out.is_empty() {
                if self.owed > 0 {
                    self.out.push_str(&"\n".repeat(self.owed as usize));
                } else if self.space {
                    self.out.push(' ');
                }
            }
            if !self.prefix.is_empty() {
                self.out.push_str(std::mem::take(&mut self.prefix));
            }
            (self.owed, self.space) = (0, false);
            self.out.push(c);
            if let Some((_, t)) = &mut self.link {
                t.push(c);
            }
        }
    }

    fn open(&mut self, tag: &html5ever::tokenizer::Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;
        let attr = |n: &str| {
            tag.attrs
                .iter()
                .find(|a| &*a.name.local == n)
                .map(|a| a.value.to_string())
        };
        // The tokenizer is told the mode even inside a skipped element, or a `<style>` in the head
        // would be read as markup.
        let feedback = match name {
            "script" => TokenSinkResult::RawData(RawKind::ScriptData),
            "style" => TokenSinkResult::RawData(RawKind::Rawtext),
            "title" => TokenSinkResult::RawData(RawKind::Rcdata),
            _ => TokenSinkResult::Continue,
        };
        if self.skip.is_some() && name != "body" {
            return feedback;
        }
        match name {
            "script" | "style" | "title" | "head" => {
                self.skip = Some(name.to_owned());
            }
            "body" => self.skip = None,
            "br" => self.breaks(1),
            "hr" => {
                self.breaks(2);
                self.text(RULE);
                self.breaks(2);
            }
            "img" => {
                if let Some(alt) = attr("alt").filter(|a| !a.trim().is_empty()) {
                    self.text(&format!("[{}]", alt.trim()));
                }
            }
            "a" => self.link = attr("href").map(|h| (h, String::new())),
            "li" => {
                self.breaks(1);
                self.prefix = "- ";
            }
            "p" | "div" | "table" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "blockquote" | "section" | "article" | "header" | "footer" | "pre" | "center" => {
                self.breaks(2)
            }
            "tr" | "td" | "th" | "dl" | "dt" | "dd" | "form" | "address" | "main" | "nav" => {
                self.breaks(1)
            }
            _ => {}
        }
        feedback
    }

    fn close(&mut self, name: &str) {
        if self.skip.as_deref() == Some(name) {
            self.skip = None;
            return;
        }
        match name {
            "a" => {
                if let Some((href, text)) = self.link.take() {
                    let (text, href) = (text.trim(), href.trim());
                    let shown = ["http://", "https://", "mailto:", "tel:"]
                        .iter()
                        .any(|s| href.to_ascii_lowercase().starts_with(s));
                    if shown && text != href && text != href.trim_start_matches("mailto:") {
                        self.text(&format!(" ({href})"));
                    }
                }
            }
            "p" | "div" | "table" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "blockquote" | "section" | "article" | "header" | "footer" | "pre" | "center" => {
                self.breaks(2)
            }
            "tr" | "td" | "th" | "li" | "dl" | "dt" | "dd" | "form" | "address" | "main"
            | "nav" => self.breaks(1),
            _ => {}
        }
    }
}

struct Sink {
    st: RefCell<Flow>,
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut st = self.st.borrow_mut();
        match token {
            Token::CharacterTokens(t) if st.skip.is_none() => st.text(&t),
            Token::TagToken(tag) => match tag.kind {
                StartTag => return st.open(&tag),
                EndTag => st.close(&tag.name),
            },
            // Comments, doctypes, NULs and everything inside a skipped element are not content.
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::custom_fields::{Definition, FieldKind};
    use crate::template::block::{Mark, Run};
    use crate::template::mjml::{BrandKit, CompileError, compile_blocks};
    use crate::template::render::{MARK_START, RenderError, substitute};
    use crate::template::tag::TagError;

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

    fn registry() -> Registry {
        Registry::from_definitions(&[def("country"), def("nickname")])
    }

    fn contact<'a>(first_name: &'a str, attrs: &'a Value) -> Contact<'a> {
        Contact {
            email: "ada@example.com",
            first_name,
            last_name: "Lovelace",
            timezone: None,
            attributes: attrs,
        }
    }

    fn text(s: &str) -> Run {
        Run::Text {
            text: s.into(),
            marks: vec![],
        }
    }

    fn tag(s: &str) -> Run {
        Run::Tag { source: s.into() }
    }

    fn para(runs: Vec<Run>) -> Block {
        Block::Text { runs }
    }

    fn button(label: &str, href: &str) -> Block {
        Block::Button {
            label: label.into(),
            href: href.into(),
        }
    }

    fn text_template(blocks: &[Block], brand: &BrandKit) -> TextTemplate {
        compile_text(blocks, brand, &registry()).expect("the template must compile")
    }

    fn render_as(first_name: &str, attrs: &Value, blocks: &[Block]) -> String {
        render_text(
            &text_template(blocks, &BrandKit::default()),
            &contact(first_name, attrs),
        )
        .expect("the template must render")
    }

    fn render(blocks: &[Block]) -> String {
        render_as("Ada", &json!({}), blocks)
    }

    const ENTITIES: [&str; 8] = [
        "&amp;", "&lt;", "&gt;", "&quot;", "&#39;", "&#34;", "&#123;", "&nbsp;",
    ];

    fn no_entities(s: &str) {
        for e in ENTITIES {
            assert!(!s.contains(e), "{e} in plain text: {s:?}");
        }
    }

    #[test]
    fn a_heading_becomes_a_line_not_a_tag() {
        let out = render(&[
            Block::Heading {
                level: 1,
                runs: vec![text("Big news")],
            },
            para(vec![text("Body.")]),
        ]);
        assert_eq!(out, "Big news\n\nBody.");
    }

    #[test]
    fn a_button_becomes_its_label_and_its_url() {
        let out = render(&[button("Get started", "https://app.example/start")]);
        assert_eq!(out, "Get started: https://app.example/start");
    }

    #[test]
    fn a_button_whose_link_is_unusable_keeps_only_its_label() {
        for href in ["javascript:alert(1)", "JaVa\tScRiPt:alert(1)", ""] {
            assert_eq!(render(&[button("Go", href)]), "Go", "{href:?}");
        }
    }

    #[test]
    fn a_link_run_shows_where_it_points() {
        let link = |text: &str, href: &str| Run::Link {
            href: href.into(),
            text: text.into(),
            marks: vec![],
        };
        assert_eq!(
            render(&[para(vec![
                text("Read the "),
                link("docs", "https://a.example/docs"),
                text(" now."),
            ])]),
            "Read the docs (https://a.example/docs) now."
        );
        // Not "https://a.example/ (https://a.example/)".
        assert_eq!(
            render(&[para(vec![link("https://a.example/", "https://a.example/")])]),
            "https://a.example/"
        );
        assert_eq!(
            render(&[para(vec![link(
                "ada@example.com",
                "mailto:ada@example.com"
            )])]),
            "ada@example.com"
        );
        assert_eq!(
            render(&[para(vec![link("click", "javascript:alert(1)")])]),
            "click"
        );
    }

    #[test]
    fn an_image_becomes_its_alt_text() {
        let image = |alt: &str| Block::Image {
            src: "https://a.example/i.png".into(),
            alt: alt.into(),
        };
        let out = render(&[image("Spring sale banner")]);
        assert_eq!(out, "[Spring sale banner]");
        assert!(!out.contains("i.png"), "{out}");
        // A decorative image has nothing to say, and must not leave an empty `[]` or a gap.
        assert_eq!(
            render(&[para(vec![text("a")]), image(""), para(vec![text("b")])]),
            "a\n\nb"
        );
    }

    #[test]
    fn a_divider_is_a_rule_and_a_spacer_is_nothing() {
        let out = render(&[
            para(vec![text("a")]),
            Block::Divider {},
            Block::Spacer { height: 40 },
            para(vec![text("b")]),
        ]);
        assert_eq!(out, format!("a\n\n{}\n\nb", "-".repeat(40)));
    }

    #[test]
    fn columns_read_one_after_another() {
        let out = render(&[Block::Columns {
            children: vec![
                vec![para(vec![text("left")]), button("L", "https://a.example/l")],
                vec![para(vec![text("right")])],
            ],
        }]);
        assert_eq!(out, "left\n\nL: https://a.example/l\n\nright");
    }

    #[test]
    fn a_newline_in_run_text_is_already_a_newline() {
        assert_eq!(
            render(&[para(vec![text("line one\nline two")])]),
            "line one\nline two"
        );
    }

    #[test]
    fn the_output_contains_no_html_entities() {
        let nasty = "Tom & Jerry <b>\"q\"</b> it's";
        let attrs = json!({"country": nasty});
        let out = render_as(
            nasty,
            &attrs,
            &[
                Block::Heading {
                    level: 2,
                    runs: vec![text(nasty)],
                },
                para(vec![
                    text(nasty),
                    tag("{{ first_name }}"),
                    tag("{{ country }}"),
                    tag("{{ nickname | \"it's <fine> & good\" }}"),
                    Run::Link {
                        href: "https://a.example/?a=1&b=2&c=3".into(),
                        text: nasty.into(),
                        marks: vec![],
                    },
                ]),
                button(nasty, "https://a.example/go?x=1&y=2"),
                Block::Image {
                    src: "https://a.example/i.png".into(),
                    alt: nasty.into(),
                },
                // Typed brace syntax is literal text (contract section 13), and still no entity.
                para(vec![text("{{ first_name }}")]),
            ],
        );
        no_entities(&out);
        // The characters themselves arrive, once per place they were written.
        assert!(out.matches(nasty).count() >= 6, "{out}");
        assert!(out.contains("it's <fine> & good"), "{out}");
        assert!(out.contains("https://a.example/?a=1&b=2&c=3"), "{out}");
        assert!(out.contains("https://a.example/go?x=1&y=2"), "{out}");
        assert!(out.contains("{{ first_name }}"), "{out}");
    }

    #[test]
    fn a_value_that_spells_an_entity_is_delivered_as_typed() {
        // Decoding the escaping once is exact; decoding it twice would turn this into `<` and `&`.
        let typed = "&lt;b&gt; &amp; &#39;";
        let attrs = json!({"country": typed});
        let out = render_as(
            typed,
            &attrs,
            &[para(vec![
                tag("{{ first_name }}|{{ country }}"),
                text(typed),
            ])],
        );
        assert_eq!(out, format!("{typed}|{typed}{typed}"));
    }

    #[test]
    fn the_output_contains_no_markup() {
        let marked = |s: &str| Run::Text {
            text: s.into(),
            marks: vec![Mark::Bold, Mark::Italic],
        };
        let out = render(&[
            Block::Heading {
                level: 1,
                runs: vec![marked("Hello")],
            },
            para(vec![
                marked("bold and italic"),
                tag("{{ first_name }}"),
                Run::Link {
                    href: "https://a.example/".into(),
                    text: "a link".into(),
                    marks: vec![Mark::Bold],
                },
            ]),
            button("Go", "https://a.example/go"),
            Block::Divider {},
            Block::Image {
                src: "https://a.example/i.png".into(),
                alt: "alt".into(),
            },
        ]);
        for needle in ["<", ">", "&", "**", "__", "href=", "style="] {
            assert!(!out.contains(needle), "{needle:?} in {out:?}");
        }
        // Marks are not shown: emphasis in plain text would be markup of our own invention.
        assert!(out.contains("bold and italic"), "{out}");
    }

    fn escaped(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[test]
    fn a_tag_in_a_run_takes_the_value_the_html_part_takes() {
        let blocks = [para(vec![
            text("Hi "),
            tag("{{ first_name | upper }}"),
            text(" / "),
            tag("{{ nickname | \"there\" }}"),
            text(" / "),
            tag("{{# country }}in {{ country }}{{/ country }}"),
            tag("{{^ country }}nowhere{{/ country }}"),
        ])];
        let reg = registry();
        let compiled = compile_blocks(&blocks, &BrandKit::default(), &reg).unwrap();
        let compiled_text = text_template(&blocks, &BrandKit::default());
        for (name, attrs, want) in [
            ("Ada & <Bob>", json!({}), "Hi ADA & <BOB> / there / nowhere"),
            (
                "Ada",
                json!({"country": "UK & IE"}),
                "Hi ADA / there / in UK & IE",
            ),
        ] {
            let c = contact(name, &attrs);
            let out = render_text(&compiled_text, &c).unwrap();
            assert_eq!(out, want);
            // The HTML part, computed from the same AST and contact, agrees about every value.
            let html = substitute(&compiled.html, &compiled.nodes, &c).unwrap();
            assert!(html.contains(&escaped(want)), "{html}");
        }
    }

    #[test]
    fn a_tag_in_a_url_field_is_encoded_for_the_url_like_the_html_part() {
        let blocks = [button(
            "Go",
            "https://app.example/u/{{ first_name }}?e={{ email }}",
        )];
        let reg = registry();
        let attrs = json!({});
        let c = contact("A B&C", &attrs);
        let out = render_text(&text_template(&blocks, &BrandKit::default()), &c).unwrap();
        assert_eq!(out, "Go: https://app.example/u/A%20B&C?e=ada@example.com");
        let compiled = compile_blocks(&blocks, &BrandKit::default(), &reg).unwrap();
        let html = substitute(&compiled.html, &compiled.nodes, &c).unwrap();
        assert!(
            html.contains("https://app.example/u/A%20B&amp;C?e=ada@example.com"),
            "{html}"
        );
    }

    #[test]
    fn a_tag_that_resolves_to_an_unsafe_url_is_dropped_like_the_html_part_drops_it() {
        let blocks = [button("Go", "{{ nickname }}")];
        let attrs = json!({"nickname": "javascript:alert(1)"});
        assert_eq!(render_as("Ada", &attrs, &blocks), "Go");
        let attrs = json!({"nickname": "https://a.example/ok"});
        assert_eq!(
            render_as("Ada", &attrs, &blocks),
            "Go: https://a.example/ok"
        );
    }

    #[test]
    fn a_conditional_may_span_blocks() {
        let blocks = [
            para(vec![tag("{{# country }}")]),
            para(vec![text("only for people with a country")]),
            para(vec![tag("{{/ country }}")]),
            para(vec![text("everyone")]),
        ];
        assert_eq!(render(&blocks), "everyone");
        assert_eq!(
            render_as("Ada", &json!({"country": "UK"}), &blocks),
            "only for people with a country\n\neveryone"
        );
    }

    #[test]
    fn a_tag_naming_an_unknown_field_is_refused_at_save_as_the_html_part_refuses_it() {
        let blocks = [para(vec![text("Hello"), tag("{{ gone }}")])];
        let err = compile_text(&blocks, &BrandKit::default(), &registry()).unwrap_err();
        assert_eq!(
            err,
            CompileError::Tag(TagError::UnknownField("gone".into()))
        );
        let html = compile_blocks(&blocks, &BrandKit::default(), &registry()).unwrap_err();
        assert_eq!(err, html);
    }

    #[test]
    fn a_literal_marker_is_refused() {
        let blocks = [para(vec![text(&format!("a {MARK_START}0~tmmn~ b"))])];
        assert_eq!(
            compile_text(&blocks, &BrandKit::default(), &registry()).unwrap_err(),
            CompileError::ForgedMarker
        );
    }

    #[test]
    fn an_invalid_brand_kit_is_refused() {
        let brand = BrandKit {
            accent: Some("red; }".into()),
            ..BrandKit::default()
        };
        assert!(matches!(
            compile_text(&[], &brand, &registry()),
            Err(CompileError::Brand(_))
        ));
    }

    #[test]
    fn a_tag_in_the_scheme_or_host_of_a_link_is_refused_at_save() {
        for href in [
            "https://{{ nickname }}.example/",
            "{{ nickname }}://a.example/",
            "javascript:{{ nickname }}",
        ] {
            let blocks = [button("Go", href)];
            assert!(
                matches!(
                    compile_text(&blocks, &BrandKit::default(), &registry()),
                    Err(CompileError::Render(RenderError::DangerousContext { .. }))
                ),
                "{href}"
            );
        }
    }

    #[test]
    fn the_footer_and_its_postal_address_are_in_the_text_part() {
        let brand = BrandKit {
            footer_text: Some("Acme Ltd, you get this because you signed up.".into()),
            footer_address: Some("1 Main St, Springfield".into()),
            ..BrandKit::default()
        };
        let out = render_text(
            &text_template(&[para(vec![text("Body.")])], &brand),
            &contact("Ada", &json!({})),
        )
        .unwrap();
        assert_eq!(
            out,
            "Body.\n\nAcme Ltd, you get this because you signed up.\n1 Main St, Springfield"
        );
        // Either half alone, and a blank one is not printed.
        let only_address = BrandKit {
            footer_text: Some("  ".into()),
            footer_address: Some("1 Main St".into()),
            ..BrandKit::default()
        };
        let out = render_text(
            &text_template(&[para(vec![text("Body.")])], &only_address),
            &contact("Ada", &json!({})),
        )
        .unwrap();
        assert_eq!(out, "Body.\n\n1 Main St");
    }

    #[test]
    fn a_footer_that_is_all_there_is_has_no_leading_gap() {
        let brand = BrandKit {
            footer_address: Some("1 Main St".into()),
            ..BrandKit::default()
        };
        let out = render_text(&text_template(&[], &brand), &contact("Ada", &json!({}))).unwrap();
        assert_eq!(out, "1 Main St");
    }

    #[test]
    fn typed_brace_syntax_in_the_footer_is_literal() {
        let brand = BrandKit {
            footer_text: Some("Hi {{ first_name }}".into()),
            ..BrandKit::default()
        };
        let out = render_text(&text_template(&[], &brand), &contact("Ada", &json!({}))).unwrap();
        assert_eq!(out, "Hi {{ first_name }}");
    }

    #[test]
    fn a_substituted_value_with_markup_characters_arrives_verbatim() {
        let attrs = json!({"country": "R&D <team> \"x\" it's"});
        let out = render_as(
            "Ada <&> Lovelace",
            &attrs,
            &[
                para(vec![tag("{{ first_name }}|{{ country }}")]),
                button("Go", "https://a.example/?n={{ first_name }}"),
            ],
        );
        // The URL is percent-encoded for the component, exactly as the HTML part's href is; the
        // text is not encoded at all.
        assert_eq!(
            out,
            "Ada <&> Lovelace|R&D <team> \"x\" it's\n\nGo: https://a.example/?n=Ada%20%3C%26%3E%20Lovelace"
        );
    }

    #[test]
    fn a_conditional_hides_the_same_branch_the_html_part_hides() {
        let blocks = [
            para(vec![
                text("before "),
                tag("{{# country }}"),
                text("HAS-COUNTRY"),
                tag("{{/ country }}"),
                tag("{{^ country }}NO-COUNTRY{{/ country }}"),
                tag("{{# country = \"UK\" }}IS-UK{{/ country }}"),
            ]),
            button("Go", "https://a.example/{{# country }}x{{/ country }}"),
        ];
        let reg = registry();
        let html_t = compile_blocks(&blocks, &BrandKit::default(), &reg).unwrap();
        let text_t = text_template(&blocks, &BrandKit::default());
        for attrs in [
            json!({}),
            json!({"country": "UK"}),
            json!({"country": "FR"}),
        ] {
            let c = contact("Ada", &attrs);
            let html = substitute(&html_t.html, &html_t.nodes, &c).unwrap();
            let out = render_text(&text_t, &c).unwrap();
            for branch in ["HAS-COUNTRY", "NO-COUNTRY", "IS-UK"] {
                assert_eq!(
                    out.contains(branch),
                    html.contains(branch),
                    "{branch} for {attrs}: {out:?}"
                );
            }
            assert_eq!(
                out.contains("https://a.example/x"),
                html.contains("https://a.example/x"),
                "{attrs}"
            );
        }
        assert_eq!(
            render_text(&text_t, &contact("Ada", &json!({"country": "UK"}))).unwrap(),
            "before HAS-COUNTRYIS-UK\n\nGo: https://a.example/x"
        );
    }

    #[test]
    fn the_stored_template_needs_no_registry_to_render() {
        // Compile under a registry that knows `country`, then render with nothing but the
        // template and a contact: a field archived since the save cannot change the answer.
        let blocks = [para(vec![tag("{{ country | \"nowhere\" }}")])];
        let t = text_template(&blocks, &BrandKit::default());
        let attrs = json!({"country": "UK"});
        assert_eq!(render_text(&t, &contact("Ada", &attrs)).unwrap(), "UK");
        assert_eq!(
            render_text(&t, &contact("Ada", &json!({}))).unwrap(),
            "nowhere"
        );
    }

    #[test]
    fn a_conditional_inside_a_dropped_conditional_does_not_end_the_drop_early() {
        let nested = [para(vec![
            tag("{{# nickname }}a{{# country }}b{{/ country }}c{{/ nickname }}"),
            text("z"),
        ])];
        // Outer dropped, inner dropped; outer dropped, inner kept; outer kept, inner dropped.
        assert_eq!(render_as("Ada", &json!({}), &nested), "z");
        assert_eq!(render_as("Ada", &json!({"country": "UK"}), &nested), "z");
        assert_eq!(render_as("Ada", &json!({"nickname": "A"}), &nested), "acz");
        assert_eq!(
            render_as("Ada", &json!({"nickname": "A", "country": "UK"}), &nested),
            "abcz"
        );
    }

    #[test]
    fn a_link_delimiter_anywhere_but_where_the_builder_put_it_is_refused() {
        use crate::template::render::classify_text;
        let (o, s, c) = (TEXT_LINK_OPEN, TEXT_LINK_SEP, TEXT_LINK_CLOSE);
        for bad in [
            format!("a{s}b{o}{s}u{s}{c}"),
            format!("{o}{s}u{s}t{o}x{c}"),
            format!("{o}l{o}{s}u{s}{c}"),
        ] {
            assert!(
                matches!(
                    classify_text(&bad, 0),
                    Err(RenderError::DangerousContext { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_blank_link_text_is_just_the_url() {
        let blank = Run::Link {
            href: "https://a.example/x".into(),
            text: "  ".into(),
            marks: vec![],
        };
        assert_eq!(render(&[para(vec![blank])]), "https://a.example/x");
    }

    #[test]
    fn operator_text_next_to_a_tag_in_a_tag_run_is_still_a_literal() {
        // Text in a `Run::Tag` that is not the tag is a literal like any other text, braces included.
        let out = render(&[para(vec![tag("a{b} {{ first_name }}{c}")])]);
        assert_eq!(out, "a{b} Ada{c}");
    }

    #[test]
    fn a_button_with_no_label_is_just_its_url() {
        assert_eq!(
            render(&[button("  ", "https://a.example/go")]),
            "https://a.example/go"
        );
    }

    #[test]
    fn operator_text_may_not_contain_the_templates_own_characters() {
        // A typed marker, the stand-in for a brace and the three link delimiters are one reserved
        // class, refused one way, wherever operator text can land. Deleting or rewriting them
        // would be silent data loss.
        for reserved in [MARK_START, "\u{E000}", "\u{1}", "\u{2}", "\u{3}", "a\u{2}b"] {
            let brand_with = |t: &str| BrandKit {
                footer_text: Some(t.into()),
                ..BrandKit::default()
            };
            let cases: Vec<(&str, Vec<Block>, BrandKit)> = vec![
                (
                    "text",
                    vec![para(vec![text(reserved)])],
                    BrandKit::default(),
                ),
                (
                    "heading",
                    vec![Block::Heading {
                        level: 2,
                        runs: vec![text(reserved)],
                    }],
                    BrandKit::default(),
                ),
                (
                    "link text",
                    vec![para(vec![Run::Link {
                        href: "https://a.example/".into(),
                        text: reserved.into(),
                        marks: vec![],
                    }])],
                    BrandKit::default(),
                ),
                (
                    "link href",
                    vec![para(vec![Run::Link {
                        href: format!("https://a.example/{reserved}"),
                        text: "x".into(),
                        marks: vec![],
                    }])],
                    BrandKit::default(),
                ),
                (
                    "text next to a tag",
                    vec![para(vec![tag(&format!("{reserved}{{{{ first_name }}}}"))])],
                    BrandKit::default(),
                ),
                (
                    "button label",
                    vec![button(reserved, "https://a.example/")],
                    BrandKit::default(),
                ),
                (
                    "button href",
                    vec![button("Go", &format!("https://a.example/{reserved}"))],
                    BrandKit::default(),
                ),
                (
                    "image alt",
                    vec![Block::Image {
                        src: "https://a.example/i.png".into(),
                        alt: reserved.into(),
                    }],
                    BrandKit::default(),
                ),
                ("footer", vec![], brand_with(reserved)),
            ];
            for (place, blocks, brand) in cases {
                assert_eq!(
                    compile_text(&blocks, &brand, &registry()),
                    Err(CompileError::ForgedMarker),
                    "{place}: {reserved:?}"
                );
            }
        }
    }

    #[test]
    fn a_marker_in_a_links_lead_or_tail_is_refused_not_copied_out() {
        use crate::template::render::{MARK_END, classify_text, substitute_text};
        let (o, s, c) = (TEXT_LINK_OPEN, TEXT_LINK_SEP, TEXT_LINK_CLOSE);
        let m = |id: usize, suffix: &str| format!("{MARK_START}{id}{suffix}{MARK_END}");
        // `lead` and `tail` are the only bytes copied out without being walked: a field marker there
        // would reach the recipient as `~nmmt~0~tmmn~`, and a conditional pair there passes the
        // marker count and is never walked, so its branch is NOT suppressed.
        let in_lead = format!("{o}{}{s}https://a.example/{s}{c}", m(0, ""));
        let in_tail = format!("{o}{s}https://a.example/{s}{}{c}", m(0, ""));
        let pair_in_lead_and_tail =
            format!("{o}{}{s}https://a.example/{s}{}{c}", m(0, "o"), m(0, "c"));
        let nodes = tag::compile("{{# first_name }}{{/ first_name }}", &registry()).unwrap();
        let field = tag::compile("{{ first_name }}", &registry()).unwrap();
        for (bad, nodes) in [
            (&in_lead, &field),
            (&in_tail, &field),
            (&pair_in_lead_and_tail, &nodes),
        ] {
            assert!(
                matches!(
                    classify_text(bad, 1),
                    Err(RenderError::DangerousContext { .. })
                ),
                "classify: {bad:?}"
            );
            let attrs = json!({});
            assert!(
                matches!(
                    substitute_text(bad, nodes, &contact("", &attrs)),
                    Err(RenderError::DangerousContext { .. })
                ),
                "substitute: {bad:?}"
            );
            // Legible months later: the refusal says where the marker is, not `0`.
            let at = bad.find(MARK_START).unwrap();
            assert!(at > 0);
            assert_eq!(
                classify_text(bad, 1),
                Err(RenderError::DangerousContext { position: at }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_stored_ast_holds_the_operators_braces_not_the_stand_in() {
        // `TextTemplate.nodes` is stored. The stand-in for a brace must not be in it, at the top
        // level or inside a conditional's body.
        fn has_stand_in(nodes: &[Node]) -> bool {
            nodes.iter().any(|n| match n {
                Node::Text(t) => t.contains('\u{E000}'),
                Node::Field { .. } => false,
                Node::Present { body, .. }
                | Node::Absent { body, .. }
                | Node::Equals { body, .. } => has_stand_in(body),
            })
        }
        let t = text_template(
            &[para(vec![
                text("{{ typed }} {"),
                tag("{{# country }}in {braces} {{ country }}{{/ country }}"),
            ])],
            &BrandKit::default(),
        );
        assert!(!has_stand_in(&t.nodes), "{:?}", t.nodes);
        assert!(!t.text.contains('\u{E000}'), "{:?}", t.text);
        assert!(
            t.nodes
                .iter()
                .any(|n| matches!(n, Node::Text(s) if s.contains("{{ typed }} {")))
        );
        // And the text it renders has them.
        assert_eq!(
            render_text(&t, &contact("Ada", &json!({"country": "UK"}))).unwrap(),
            "{{ typed }} {in {braces} UK"
        );
    }

    #[test]
    fn nothing_but_empty_blocks_is_an_empty_string() {
        assert_eq!(render(&[]), "");
        assert_eq!(render(&[para(vec![]), Block::Spacer { height: 8 }]), "");
    }

    // ---- raw mode ----

    #[test]
    fn raw_mode_html_tables_become_blocks_not_one_line() {
        let html = "<table><tr><td>First cell</td><td>Second cell</td></tr>\
                    <tr><td><p>Third</p></td></tr></table><table><tr><td>Next table</td></tr></table>";
        let out = text_from_html(html);
        assert_eq!(out, "First cell\nSecond cell\n\nThird\n\nNext table");
    }

    #[test]
    fn raw_mode_decodes_entities_and_collapses_source_whitespace() {
        let out = text_from_html(
            "<p>Tom &amp; Jerry&#39;s   &lt;3\n   and\t&quot;more&quot;&nbsp;here</p>",
        );
        assert_eq!(out, "Tom & Jerry's <3 and \"more\" here");
        no_entities(&out);
    }

    #[test]
    fn raw_mode_drops_what_is_not_content() {
        let out = text_from_html(
            "<!DOCTYPE html><html><head><title>Subject line</title>\
             <style>td { color: red; }</style></head>\
             <body><script>alert('x')</script><!--[if mso]><p>outlook</p><![endif]-->\
             <p>Visible</p></body></html>",
        );
        assert_eq!(out, "Visible");
    }

    #[test]
    fn raw_mode_reads_script_and_style_as_raw_text_and_ends_an_unclosed_head_at_body() {
        // In a browser an unclosed `<!--` inside a script is script text, not a comment that
        // swallows the rest of the message.
        assert_eq!(
            text_from_html("<script>var a = \"<!--\";</script><p>Visible</p>"),
            "Visible"
        );
        assert_eq!(
            text_from_html("<style>/* <!-- */</style><p>Visible</p>"),
            "Visible"
        );
        // `</head>` is optional in HTML.
        assert_eq!(
            text_from_html("<head><title>Subject</title><body><p>Visible</p>"),
            "Visible"
        );
    }

    #[test]
    fn raw_mode_keeps_links_images_breaks_rules_and_list_items() {
        let out = text_from_html(
            "<p>See <a href=\"https://a.example/x?a=1&amp;b=2\">the page</a> or \
             <a href=\"https://b.example/\">https://b.example/</a> or \
             <a href=\"#top\">top</a> or <a href=\"javascript:x()\">js</a>.<br>Next line</p>\
             <img src=\"https://a.example/i.png\" alt=\"Banner\"><img src=\"p.gif\" alt=\"\">\
             <hr><ul><li>one</li><li>two</li></ul>",
        );
        assert_eq!(
            out,
            "See the page (https://a.example/x?a=1&b=2) or https://b.example/ or top or js.\n\
             Next line\n\n[Banner]\n\n----------------------------------------\n\n- one\n- two"
        );
    }

    #[test]
    fn raw_mode_output_has_no_markup() {
        let out = text_from_html(
            "<div style=\"x\"><table><tr><td class=\"a\"><h1>Title</h1>\
             <a href=\"https://a.example/\">go</a></td></tr></table></div>",
        );
        assert_eq!(out, "Title\n\ngo (https://a.example/)");
    }
}
