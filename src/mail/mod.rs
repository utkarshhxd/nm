// SPDX-License-Identifier: AGPL-3.0-only

//! Outgoing message construction: MIME bytes plus the headers a bulk sender must carry (spec §5.5).
//!
//! **`List-Unsubscribe` is ours, never a relay's.** OCI Email Delivery adds its own one-click
//! header when it finds no HTTPS one, and a click on *its* link suppresses the person in Oracle's
//! list and nowhere in ours: no `consent_log` row, nothing the preference centre or a GDPR export
//! can see. So every message carries an HTTPS `List-Unsubscribe` pointing at this product, with
//! `List-Unsubscribe-Post` for RFC 8058 one-click.
//!
//! DKIM signing is [`dkim`], which signs [`SIGNED_HEADERS`]. lettre's own `dkim`
//! feature stays off: it reaches RSA private-key operations through the `rsa` crate, and
//! `deny.toml`'s RUSTSEC-2023-0071 ignore rests on this product performing none.

pub mod dkim;

use lettre::{
    Address, Message,
    message::{
        Mailbox, MultiPart, SinglePart,
        header::{Header, HeaderName, HeaderValue},
    },
};
use uuid::Uuid;

/// Header names this module adds itself. Each is spelled once, here; the builder reaches them
/// through the `Header` impls below and [`SIGNED_HEADERS`] lists them, so the two cannot drift.
const LIST_UNSUBSCRIBE: &str = "List-Unsubscribe";
const LIST_UNSUBSCRIBE_POST: &str = "List-Unsubscribe-Post";
const FEEDBACK_ID: &str = "Feedback-ID";

/// RFC 8058: the one value that makes `List-Unsubscribe` a one-click endpoint.
const ONE_CLICK: &str = "List-Unsubscribe=One-Click";

/// The `h=` list the DKIM signer signs. RFC 8058 requires the signature to cover both
/// unsubscribe headers. The first group is set by lettre; a test checks each is on the wire.
pub const SIGNED_HEADERS: &[&str] = &[
    "From",
    "To",
    "Subject",
    "Date",
    "Message-ID",
    "MIME-Version",
    "Content-Type",
    LIST_UNSUBSCRIBE,
    LIST_UNSUBSCRIBE_POST,
    FEEDBACK_ID,
];

macro_rules! text_header {
    ($ty:ident, $name:expr) => {
        #[derive(Clone)]
        struct $ty(String);

        impl Header for $ty {
            fn name() -> HeaderName {
                HeaderName::new_from_ascii_str($name)
            }
            fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
                Ok(Self(s.to_owned()))
            }
            fn display(&self) -> HeaderValue {
                HeaderValue::new(Self::name(), self.0.clone())
            }
        }
    };
}
text_header!(ListUnsubscribeHeader, LIST_UNSUBSCRIBE);
text_header!(ListUnsubscribePostHeader, LIST_UNSUBSCRIBE_POST);
text_header!(FeedbackIdHeader, FEEDBACK_ID);

pub struct Outgoing<'a> {
    pub from: &'a str,
    pub from_name: Option<&'a str>,
    pub to: &'a str,
    pub subject: &'a str,
    pub text: &'a str,
    pub html: Option<&'a str>,
    pub unsubscribe_url: &'a url::Url,
    pub feedback_id: &'a str,
    pub message_id_domain: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// Refused, not punycoded: that needs another dependency, and relays' approved-sender rules
    /// are ASCII anyway. The caller fails this one message.
    #[error("{0} address must be ASCII; internationalised addresses are not supported")]
    NonAsciiAddress(&'static str),
    #[error("{0} address is not a valid email address")]
    InvalidAddress(&'static str),
    #[error("unsubscribe URL must be https")]
    UnsubscribeNotHttps,
    #[error("{0} must be printable ASCII with no line breaks")]
    UnsafeHeaderValue(&'static str),
    #[error("could not assemble the message: {0}")]
    Build(#[from] lettre::error::Error),
}

/// Printable ASCII only. Rules out CR, LF and every control character, so a value cannot end its
/// header early and start another.
fn safe(what: &'static str, v: &str) -> Result<(), MailError> {
    if v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        Ok(())
    } else {
        Err(MailError::UnsafeHeaderValue(what))
    }
}

fn address(class: &'static str, raw: &str) -> Result<Address, MailError> {
    // lettre would accept an IDN domain here; this product refuses it instead.
    if !raw.is_ascii() {
        return Err(MailError::NonAsciiAddress(class));
    }
    raw.parse().map_err(|_| MailError::InvalidAddress(class))
}

pub fn build(o: &Outgoing) -> Result<Message, MailError> {
    if o.unsubscribe_url.scheme() != "https" {
        return Err(MailError::UnsubscribeNotHttps);
    }
    safe("feedback id", o.feedback_id)?;
    // The domain goes inside `<…@…>` unquoted, so it is held to hostname characters, not just ASCII.
    if o.message_id_domain.is_empty()
        || !o
            .message_id_domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(MailError::UnsafeHeaderValue("message-id domain"));
    }

    let from = Mailbox::new(o.from_name.map(str::to_owned), address("sender", o.from)?);
    let to = Mailbox::new(None, address("recipient", o.to)?);

    let builder = Message::builder()
        .from(from)
        .to(to)
        .subject(o.subject)
        .date_now()
        .message_id(Some(format!(
            "<{}@{}>",
            Uuid::now_v7().simple(),
            o.message_id_domain
        )))
        .header(ListUnsubscribeHeader(format!("<{}>", o.unsubscribe_url)))
        .header(ListUnsubscribePostHeader(ONE_CLICK.to_owned()))
        .header(FeedbackIdHeader(o.feedback_id.to_owned()));

    Ok(match o.html {
        Some(html) => builder.multipart(MultiPart::alternative_plain_html(
            o.text.to_owned(),
            html.to_owned(),
        ))?,
        None => builder.singlepart(SinglePart::plain(o.text.to_owned()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> url::Url {
        "https://example.test/u/TOK".parse().unwrap()
    }

    fn outgoing(unsubscribe_url: &url::Url) -> Outgoing<'_> {
        Outgoing {
            from: "news@brand.test",
            from_name: None,
            to: "reader@dest.test",
            subject: "Hello",
            text: "plain body",
            html: None,
            unsubscribe_url,
            feedback_id: "camp1:list1:nm",
            message_id_domain: "send.example.test",
        }
    }

    fn wire(o: &Outgoing) -> String {
        String::from_utf8(build(o).unwrap().formatted()).unwrap()
    }

    /// The value of the first header called `name`, from rendered bytes (header block only).
    fn header(wire: &str, name: &str) -> Option<String> {
        let head = wire.split("\r\n\r\n").next().unwrap();
        head.split("\r\n").find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }

    #[test]
    fn both_unsubscribe_headers_are_present_and_ours() {
        let u = url();
        let w = wire(&outgoing(&u));
        assert!(
            w.contains("\r\nList-Unsubscribe: <https://example.test/u/TOK>\r\n"),
            "{w}"
        );
        assert!(
            w.contains("\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n"),
            "{w}"
        );
        assert!(!w.to_ascii_lowercase().contains("mailto:"), "{w}");
        assert_eq!(
            header(&w, "Feedback-ID").as_deref(),
            Some("camp1:list1:nm"),
            "{w}"
        );
    }

    /// **The message goes to the person it was addressed to, and says what it was asked to say.**
    ///
    /// This looks too obvious to test, and it was missed for exactly that reason. Every other test
    /// here checks a header the module is *interesting* about — the unsubscribe pair, `Feedback-ID`,
    /// `Message-ID` — and none checked the two that carry the actual message. A review hunting for
    /// a mutation nothing would catch found it in seconds: replace the recipient with the sender's
    /// own mailbox, *after* validating the real one, and all eleven tests still pass. A message
    /// delivered to the wrong person, with the suite green.
    ///
    /// The plain part is asserted through the body rather than `header`, since in a
    /// `multipart/alternative` it is not in the header block.
    #[test]
    fn the_recipient_subject_and_body_are_the_ones_asked_for() {
        let u = url();
        let o = outgoing(&u);
        let w = wire(&o);
        assert_eq!(header(&w, "To").as_deref(), Some("reader@dest.test"), "{w}");
        assert_eq!(header(&w, "Subject").as_deref(), Some("Hello"), "{w}");
        assert!(w.contains("plain body"), "the text part is missing: {w}");
        // And not quietly sent from the recipient, nor to the sender.
        assert_eq!(
            header(&w, "From").as_deref(),
            Some("news@brand.test"),
            "{w}"
        );
    }

    #[test]
    fn signed_headers_covers_both_unsubscribe_headers() {
        for want in ["List-Unsubscribe", "List-Unsubscribe-Post"] {
            assert!(
                SIGNED_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(want)),
                "{want}"
            );
        }
    }

    #[test]
    fn every_signed_header_is_on_the_wire() {
        let u = url();
        let w = wire(&outgoing(&u));
        for h in SIGNED_HEADERS {
            assert!(header(&w, h).is_some(), "{h} is signed but not sent:\n{w}");
        }
    }

    #[test]
    fn message_id_is_on_the_sending_domain() {
        let u = url();
        let o = outgoing(&u);
        let (a, b) = (wire(&o), wire(&o));
        let id = |w: &str| header(w, "Message-ID").unwrap();
        let (ida, idb) = (id(&a), id(&b));
        let inner = ida
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))
            .expect(&ida);
        let (lhs, rhs) = inner.split_once('@').unwrap();
        assert_eq!(rhs, "send.example.test");
        assert!(!lhs.is_empty());
        let lhs_b = idb.trim_matches(['<', '>']).split_once('@').unwrap().0;
        assert_ne!(lhs, lhs_b);
    }

    #[test]
    fn a_plain_text_part_always_exists() {
        let u = url();
        let w = wire(&outgoing(&u));
        assert!(
            header(&w, "Content-Type")
                .unwrap()
                .starts_with("text/plain"),
            "{w}"
        );
        assert!(w.contains("plain body"), "{w}");
    }

    #[test]
    fn html_makes_it_multipart_alternative_with_plain_first() {
        let u = url();
        let mut o = outgoing(&u);
        o.html = Some("<p>html body</p>");
        let w = wire(&o);
        assert!(
            header(&w, "Content-Type")
                .unwrap()
                .starts_with("multipart/alternative"),
            "{w}"
        );
        let plain = w.find("text/plain").expect("plain part");
        let html = w.find("text/html").expect("html part");
        assert!(plain < html, "{w}");
    }

    #[test]
    fn a_non_ascii_display_name_is_encoded() {
        let u = url();
        let mut o = outgoing(&u);
        o.from_name = Some("Grüße");
        let w = wire(&o);
        assert!(w.is_ascii(), "raw non-ASCII on the wire:\n{w}");
        let from = header(&w, "From").unwrap();
        assert!(from.starts_with("=?utf-8?"), "{from}");
        assert!(from.ends_with("<news@brand.test>"), "{from}");
    }

    #[test]
    fn an_idn_recipient_domain_is_rejected_with_a_reason() {
        let u = url();
        let mut o = outgoing(&u);
        o.to = "user@exämple.test";
        let e = build(&o).unwrap_err();
        assert!(
            matches!(e, MailError::NonAsciiAddress("recipient")),
            "{e:?}"
        );
        assert!(e.to_string().contains("recipient"), "{e}");
        o.to = "reader@dest.test";
        o.from = "news@exämple.test";
        assert!(matches!(
            build(&o).unwrap_err(),
            MailError::NonAsciiAddress("sender")
        ));
    }

    #[test]
    fn an_http_or_mailto_unsubscribe_url_is_refused() {
        for bad in ["http://example.test/u/T", "mailto:u@example.test"] {
            let u: url::Url = bad.parse().unwrap();
            assert!(
                matches!(build(&outgoing(&u)), Err(MailError::UnsubscribeNotHttps)),
                "{bad}"
            );
        }
    }

    #[test]
    fn line_breaks_cannot_be_smuggled_into_headers() {
        let u = url();
        let mut o = outgoing(&u);
        o.feedback_id = "a\r\nBcc: x@y.test";
        assert!(matches!(build(&o), Err(MailError::UnsafeHeaderValue(_))));
        o.feedback_id = "ok";
        o.message_id_domain = "a.test>\r\nBcc: x@y.test";
        assert!(matches!(build(&o), Err(MailError::UnsafeHeaderValue(_))));
        o.message_id_domain = "send.example.test";
        o.subject = "hi\r\nBcc: x@y.test";
        let w = wire(&o);
        assert!(header(&w, "Bcc").is_none(), "{w}");
    }
}
