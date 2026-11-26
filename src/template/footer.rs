// SPDX-License-Identifier: AGPL-3.0-only

//! The system footer every campaign message carries: the postal address and an unsubscribe link,
//! inside the classified document (spec §4.5 rulings 4, 6 and 7; issues 48 and 51).
//!
//! ```text
//! send:  authored --mrml (mjml kinds)--> operator HTML --insertion_point--> + footer --classify--> Stored
//! ```
//!
//! The footer is product markup with one slot, [`UNSUBSCRIBE_TOKEN`], in the PATH of an absolute
//! link, `{public_url}/u/{token}`. Only `tag::Registry::system_footer()` knows that name, so no
//! operator tag can fill it, and [`insertion_point`] refuses a marker naming it anywhere in the
//! operator's document. Nothing here guesses at the operator's layout: the footer goes where the
//! tokenizer says the body ends, and a document whose end is not markup is refused.

use std::cell::RefCell;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, StartTag, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use super::UNSUBSCRIBE_TOKEN;
use super::mjml::{self, CompileError, Ctx};
use super::render;

/// What a send rendition adds to the operator's document.
#[derive(Debug, Clone, Copy)]
pub struct SystemFooter<'a> {
    /// `NM_PUBLIC_URL`. The link is `{public_url}/u/{token}`, absolute, so it works in the plain
    /// text part too (issue 48).
    pub public_url: &'a url::Url,
    /// The brand kit's postal address. Written into the footer of a raw `mjml` or `html` template;
    /// a `blocks` template already emits it from the brand kit, and gets it once.
    pub postal_address: &'a str,
}

/// Spec §4.5 ruling 7, exactly: the markup before the address and after the slot.
const OPEN: &str = r#"<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr><td align="center" style="padding:16px 24px;font-family:Arial,Helvetica,sans-serif;font-size:12px;line-height:18px;color:#6b7280;">"#;
const CLOSE: &str =
    r#"" style="color:#6b7280;text-decoration:underline;">Unsubscribe</a></td></tr></table>"#;

impl SystemFooter<'_> {
    /// `public_url` without its trailing `/`, as `worker::confirmation` builds its own links.
    pub(super) fn base(&self) -> &str {
        self.public_url.as_str().trim_end_matches('/')
    }

    /// The footer's HTML as template SOURCE, for `tag::compile` with the footer registry.
    /// Everything but the slot goes through `mjml::literal`, so neither the address nor the base
    /// can open a tag, an element or a merge tag of its own.
    pub(super) fn html(&self, address: bool) -> String {
        let mut out = String::from(OPEN);
        let addr = self.postal_address.trim();
        if address && !addr.is_empty() {
            // Contract §7's order, which `literal` keeps: escape first, then `\n` to `<br>`.
            mjml::literal(addr, Ctx::Text, &mut out);
            out.push_str("<br>");
        }
        out.push_str("<a href=\"");
        mjml::literal(self.base(), Ctx::Attr, &mut out);
        out.push_str("/u/{{ ");
        out.push_str(UNSUBSCRIBE_TOKEN);
        out.push_str(" }}");
        out.push_str(CLOSE);
        out
    }
}

/// Fed between the document's own bytes at each place an end of body could start. It is looked
/// for only in what the tokenizer emits while reading that one feed, so the same character in
/// the document itself is harmless.
const PROBE: &str = "\u{E00F}";

/// Where the footer goes in `html`, the operator's document as `mrml` produced it: immediately
/// before the last `</body>` the tokenizer reads as an end tag, or at the very end when there is
/// none. `slots` is how many markers the operator's AST has; the footer's come after them.
///
/// **Refused, `FooterHidden`:** no such end tag, and the document ends inside a comment, a
/// raw-text element (`script`, `style`, `title`, …), a tag, or after `<plaintext>` — anywhere the
/// footer would be text rather than markup.
///
/// **Refused, `ForgedMarker`:** a marker anywhere in the document naming a slot at or past
/// `slots`. A typed one is already refused, but a character reference (`&#126;nmmt&#126;…`)
/// reaches the classifier decoded, and there it would be a copy of the recipient's token, filled
/// by the product, in a place the operator chose.
///
/// How the position is found: a tokenizer reports no byte offsets, so the document is fed in
/// pieces split before every `</body` candidate, with a [`PROBE`] character fed after each piece.
/// The tokenizer emits the probe as character data in the data state only when that position is
/// outside every comment, tag and raw-text element. A probe goes in only before a `<`, and nothing
/// in the tokenizer's grammar pairs a `<` with the character before it, so the probes change no
/// token around them.
pub(super) fn insertion_point(html: &str, slots: usize) -> Result<usize, CompileError> {
    let lower = html.to_ascii_lowercase();
    // `</body` and a character that ends a tag name: the only spellings read as `body`'s end tag.
    let ends = lower.match_indices("</body").map(|(i, _)| i).filter(|&i| {
        matches!(
            lower.as_bytes().get(i + "</body".len()),
            Some(b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/' | b'>')
        )
    });
    let tok = Tokenizer::new(
        Probe {
            st: RefCell::new(St {
                slots,
                ..St::default()
            }),
        },
        TokenizerOpts::default(),
    );
    let input = BufferQueue::default();
    let mut fed = 0;
    let mut markup_at = |at: usize| {
        if at > fed {
            input.push_back(StrTendril::from(&html[fed..at]));
            let _ = tok.feed(&input);
            fed = at;
        }
        tok.sink.st.borrow_mut().probing = true;
        input.push_back(StrTendril::from(PROBE));
        let _ = tok.feed(&input);
        let mut st = tok.sink.st.borrow_mut();
        st.probing = false;
        std::mem::take(&mut st.hit)
    };
    let body = ends.filter(|&at| markup_at(at)).last();
    // Always fed: the marker check below needs the whole document read.
    let end = markup_at(html.len());
    tok.end();
    if tok.sink.st.borrow().forged {
        return Err(CompileError::ForgedMarker);
    }
    body.or(end.then_some(html.len()))
        .ok_or(CompileError::FooterHidden)
}

struct Probe {
    st: RefCell<St>,
}

#[derive(Default)]
struct St {
    slots: usize,
    /// Inside a raw-text element: what the tokenizer reads here is not markup.
    raw: bool,
    /// After `<plaintext>`, which a browser reads as text to the end of the document. The
    /// tokenizer is NOT switched into that state, so it reads every later marker exactly as the
    /// classifier will; only the footer is kept out.
    plaintext: bool,
    probing: bool,
    hit: bool,
    /// A run of character data, checked whole, as the classifier buffers it.
    text: String,
    forged: bool,
}

impl St {
    fn check(&mut self, s: &str) {
        if render::marker_indices(s).any(|id| id >= self.slots) {
            self.forged = true;
        }
    }

    fn flush(&mut self) {
        let text = std::mem::take(&mut self.text);
        self.check(&text);
    }
}

impl TokenSink for Probe {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut st = self.st.borrow_mut();
        match token {
            Token::CharacterTokens(t) => {
                if st.probing && !st.raw && !st.plaintext && t.contains(PROBE) {
                    st.hit = true;
                }
                st.text.push_str(&t);
            }
            Token::NullCharacterToken => st.text.push('\u{FFFD}'),
            Token::TagToken(tag) => {
                st.flush();
                st.check(&tag.name);
                for a in &tag.attrs {
                    st.check(&a.name.local);
                    st.check(&a.value);
                }
                if tag.kind != StartTag {
                    st.raw = false;
                    return TokenSinkResult::Continue;
                }
                if &*tag.name == "plaintext" {
                    st.plaintext = true;
                }
                // The classifier's own table, so the two cannot disagree about what is markup.
                if let Some(kind) = render::raw_kind(&tag.name) {
                    st.raw = true;
                    return TokenSinkResult::RawData(kind);
                }
            }
            Token::CommentToken(t) => {
                st.flush();
                st.check(&t);
            }
            Token::DoctypeToken(d) => {
                st.flush();
                for part in [&d.name, &d.public_id, &d.system_id].into_iter().flatten() {
                    st.check(part);
                }
            }
            Token::ParseError(_) | Token::EOFToken => st.flush(),
        }
        TokenSinkResult::Continue
    }
}
