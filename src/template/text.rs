// SPDX-License-Identifier: AGPL-3.0-only

//! The plain-text part of a message.
//!
//! Plain text receives CHARACTERS, never entities. listmonk runs its alternative body through
//! `html/template` and delivers `&amp;` and `&#39;` to people reading plain text;
//! `the_output_contains_no_html_entities` is the test that stops this product repeating it.
//!
//! # Two sources, one lookup
//!
//! [`text_from_blocks`] reads the BLOCK TREE, never the rendered HTML: the tree knows a heading is a
//! heading and a button is a call to action, where the HTML knows only tables. It takes its merge
//! tags through the same `tag::compile` and the same `render::substitute` as the HTML part, so a
//! value (and a URL's percent-encoding) cannot differ between the two parts of one message. Only
//! the encoding differs: the document handed to `substitute` is text, `substitute` HTML-escapes
//! the values it writes, and [`unescape`] undoes exactly that, which is lossless because
//! `substitute` escapes `&` in every context.
//!
//! [`text_from_html`] is the raw-mode fallback, where there is no tree. Its input is the HTML that
//! is actually being sent (already substituted), so there is no merge tag left to resolve.
//!
//! # What each block looks like
//!
//! Blocks are separated by one blank line. Bold and italic are dropped: `*x*` in plain text is
//! markup of our own invention.
//!
//! * Heading: its own line. Levels are not told apart; underlining needs the substituted width.
//! * Button: `Label: URL`. There is nothing to click, so the URL has to be on the page.
//! * Link run: `text (URL)`, or the bare URL when the text already is the URL.
//! * Image: `[alt]`, nothing at all when the alt is empty. The brackets mark it as a picture
//!   rather than copy.
//! * Divider: a rule of 40 hyphens. Spacer: nothing; the blank line between blocks is the space.
//! * Columns: read in order, column after column.
//!
//! A URL the HTML part would disable (`javascript:`, userinfo, unparsable) is left out here too:
//! the label stays, the link does not.

use std::cell::RefCell;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, EndTag, StartTag, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use super::block::{Block, Run};
use super::render::{self, Contact};
use super::tag::{self, Registry};

const RULE: &str = "----------------------------------------";

/// The plain-text part of a `blocks` template for one contact.
///
/// `registry` is needed because a `Run::Tag` crosses the wire as source text and the compiled AST
/// is never stored per run; it must be the registry the template was saved under, or a field
/// archived since then makes its tag compile-fail (the tag is then left out, with a warning, not
/// the message). The brand kit's footer is not in the tree and so is not in this text: the caller
/// appends it.
pub fn text_from_blocks(blocks: &[Block], contact: &Contact<'_>, registry: &Registry) -> String {
    let build = |tags: bool| {
        let cx = Cx {
            registry,
            contact,
            tags,
        };
        let mut src = String::new();
        for b in blocks {
            block(b, &cx, &mut src);
            src.push_str("\n\n");
        }
        fill(&src, &cx)
    };
    // If the tags will not compile, say so and send the text without them rather than none.
    let out = build(true).or_else(|| build(false)).unwrap_or_default();
    tidy(&unescape(&out))
}

struct Cx<'a> {
    registry: &'a Registry,
    contact: &'a Contact<'a>,
    /// `false` only on the retry after a compile failure: every tag is dropped.
    tags: bool,
}

/// Compile `src` and substitute this contact into it, through the HTML part's own functions. The
/// result is HTML-escaped text: see [`unescape`].
fn fill(src: &str, cx: &Cx<'_>) -> Option<String> {
    let nodes = tag::compile(src, cx.registry)
        .inspect_err(|e| tracing::warn!(error = ?e, "plain text: a merge tag did not compile"))
        .ok()?;
    let (html, _) = render::marked(&nodes);
    render::substitute(&html, &nodes, cx.contact)
        .inspect_err(|e| tracing::warn!(error = ?e, "plain text: substitution was refused"))
        .ok()
}

/// A literal, written so that `fill` reads it back as exactly these characters: nothing in it can
/// open a tag (`<`), start a reference (`&`) or begin a merge tag (`{`).
fn lit(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '{' => out.push_str("&#123;"),
            c => out.push(c),
        }
    }
}

/// `s` with merge tags let through and everything else a literal. The rule is `mjml::emit`'s, byte
/// for byte: a `{{`, then the first `}}` after it.
fn emit(s: &str, tags: bool, out: &mut String) {
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        let end = start + 2 + len + 2;
        lit(&rest[..start], out);
        if tags {
            out.push_str(&rest[start..end]);
        }
        rest = &rest[end..];
    }
    lit(rest, out);
}

/// Undo `substitute`'s escaping: its text and attribute encoders write these and nothing else.
/// One pass, so `&amp;lt;` comes back as `&lt;` and not `<`.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let (c, len) = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&#34;", '"'),
            ("&#39;", '\''),
        ]
        .into_iter()
        .find_map(|(e, c)| rest.starts_with(e).then_some((c, e.len())))
        .unwrap_or(('&', 1));
        out.push(c);
        rest = &rest[len..];
    }
    out.push_str(rest);
    out
}

/// A URL field resolved for this contact exactly as the HTML part resolves an `href`: through
/// `substitute`, so a tag is percent-encoded for the component it lands in and an unsafe result
/// is disabled. `None` when there is no usable link.
fn resolve_url(href: &str, cx: &Cx<'_>) -> Option<String> {
    let mut src = String::from("<a href=\"");
    emit(href, cx.tags, &mut src);
    src.push_str("\">");
    let out = fill(&src, cx)?;
    let url = unescape(out.strip_prefix("<a href=\"")?.strip_suffix("\">")?);
    // `#` is the HTML part's disabled link.
    (!url.is_empty() && url != "#").then_some(url)
}

fn runs(rs: &[Run], cx: &Cx<'_>, out: &mut String) {
    for r in rs {
        match r {
            Run::Text { text, .. } => lit(text, out),
            Run::Tag { source } => emit(source, cx.tags, out),
            Run::Link { href, text, .. } => {
                let url = resolve_url(href, cx);
                let same =
                    |u: &str| text.trim() == u || text.trim() == u.trim_start_matches("mailto:");
                match url.filter(|u| !text.trim().is_empty() && !same(u)) {
                    Some(u) => {
                        lit(text, out);
                        out.push_str(" (");
                        lit(&u, out);
                        out.push(')');
                    }
                    None => lit(text, out),
                }
            }
        }
    }
}

fn block(b: &Block, cx: &Cx<'_>, out: &mut String) {
    match b {
        Block::Text { runs: rs } | Block::Heading { runs: rs, .. } => runs(rs, cx, out),
        Block::Image { alt, .. } => {
            if !alt.trim().is_empty() {
                out.push('[');
                lit(alt, out);
                out.push(']');
            }
        }
        Block::Button { label, href } => {
            let url = resolve_url(href, cx);
            lit(label, out);
            if let Some(u) = url {
                if !label.trim().is_empty() {
                    out.push_str(": ");
                }
                lit(&u, out);
            }
        }
        Block::Divider {} => out.push_str(RULE),
        Block::Spacer { .. } => {}
        Block::Columns { children } => {
            for child in children.iter().flatten() {
                block(child, cx, out);
                out.push_str("\n\n");
            }
        }
    }
}

/// Trailing spaces off every line, no more than one blank line in a row, nothing at either end.
/// A suppressed conditional block leaves empty paragraphs behind; this is what closes the gap.
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
    use crate::template::mjml::{BrandKit, compile_blocks};
    use crate::template::render::substitute;

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

    fn render_as(first_name: &str, attrs: &Value, blocks: &[Block]) -> String {
        text_from_blocks(blocks, &contact(first_name, attrs), &registry())
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
        for (name, attrs, want) in [
            ("Ada & <Bob>", json!({}), "Hi ADA & <BOB> / there / nowhere"),
            (
                "Ada",
                json!({"country": "UK & IE"}),
                "Hi ADA / there / in UK & IE",
            ),
        ] {
            let c = contact(name, &attrs);
            let out = text_from_blocks(&blocks, &c, &reg);
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
        let out = text_from_blocks(&blocks, &c, &reg);
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
    fn a_tag_that_cannot_compile_is_left_out_not_the_whole_message() {
        // A field archived after the template was saved: the registry no longer knows it.
        let blocks = [
            para(vec![text("Hello"), tag("{{ gone }}")]),
            button("Go", "https://a.example/"),
        ];
        assert_eq!(render(&blocks), "Hello\n\nGo: https://a.example/");
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
