// SPDX-License-Identifier: AGPL-3.0-only

//! DKIM signing (RFC 6376): `rsa-sha256`, relaxed header and body canonicalization, over the
//! fields [`SIGNED_HEADERS`] names.
//!
//! **The RSA here is `ring`'s, not a DKIM crate's.** Every Rust DKIM signer — lettre's own `dkim`
//! feature, `mail-auth`, the rest — reaches RSA private-key operations through the `rsa` crate,
//! and `deny.toml` ignores RUSTSEC-2023-0071 (the Marvin timing attack on exactly those
//! operations) on the written claim that this product performs none. Signing through `rsa` would
//! make that claim false, so this module signs with `ring`, which was already in the tree behind
//! rustls and does RSA-PKCS#1-SHA256 in constant time.
//!
//! What is left is canonicalization, which is text normalization and not cryptography. RFC 6376
//! publishes vectors for it (the examples under §3.4, over the rules in §3.4.2 and §3.4.4) and
//! the tests use them byte for byte.

use base64::{Engine, engine::general_purpose::STANDARD};
use lettre::Message;
use ring::{
    digest,
    rand::SystemRandom,
    signature::{self, RsaKeyPair},
};
use zeroize::Zeroizing;

use super::SIGNED_HEADERS;

/// A DKIM private key, ready to sign with. It has no `Debug` and no way back to its bytes: what
/// comes out of it is a signature and the public half.
pub struct SigningKey(RsaKeyPair);

#[derive(Debug, thiserror::Error)]
pub enum DkimError {
    #[error("the DKIM key is not PEM: expected a -----BEGIN …----- block with a matching END")]
    NotPem,
    #[error(
        "a PEM block labelled {0} is not an RSA private key; expected PRIVATE KEY or RSA PRIVATE KEY"
    )]
    WrongPemLabel(String),
    #[error("the DKIM key's base64 is not valid")]
    BadBase64,
    /// Both encodings are reported because the stored key is DER with the armour gone, so which
    /// of the two it was meant to be is no longer knowable. `ring`'s own reason for each names
    /// the problem: `TooSmall` for a key under 2048 bits, `InvalidEncoding` for bytes that are
    /// not an RSA private key at all.
    #[error(
        "the DKIM key is not an RSA private key this product can sign with \
         (read as PKCS#1: {pkcs1}; read as PKCS#8: {pkcs8}); it must be an unencrypted RSA key \
         of at least 2048 bits"
    )]
    UnusableKey { pkcs1: String, pkcs8: String },
    #[error("the published key is not an RSA SubjectPublicKeyInfo: {0}")]
    NotSpki(&'static str),
    #[error("the {0} reaches a mail header, so it must be hostname characters only")]
    BadName(&'static str),
    #[error("the message has no blank line between its headers and its body")]
    NoHeaderBreak,
    #[error("the DKIM signature could not be computed")]
    Sign,
}

// ---------------------------------------------------------------------------------------------
// Keys.

/// A key as an operator pastes it: PKCS#1 (`BEGIN RSA PRIVATE KEY`) or PKCS#8 (`BEGIN PRIVATE
/// KEY`), unencrypted.
pub fn key_from_pem(pem: &str) -> Result<SigningKey, DkimError> {
    let lines: Vec<&str> = pem.trim().lines().map(str::trim).collect();
    let [begin, body @ .., end] = lines.as_slice() else {
        return Err(DkimError::NotPem);
    };
    let label = begin
        .strip_prefix("-----BEGIN ")
        .and_then(|l| l.strip_suffix("-----"))
        .ok_or(DkimError::NotPem)?;
    if *end != format!("-----END {label}-----") {
        return Err(DkimError::NotPem);
    }
    if !matches!(label, "PRIVATE KEY" | "RSA PRIVATE KEY") {
        return Err(DkimError::WrongPemLabel(pem_label(label)));
    }
    let der = Zeroizing::new(
        STANDARD
            .decode(body.concat())
            .map_err(|_| DkimError::BadBase64)?,
    );
    key_from_der(&der)
}

/// A PEM label as a refusal may repeat it: the characters a label is made of, and no more. What
/// is quoted here came from whoever sent the key, and an error is shown and stored.
fn pem_label(label: &str) -> String {
    let kept: String = label
        .chars()
        .filter(|c| c.is_ascii_uppercase() || *c == ' ')
        .take(32)
        .collect();
    if kept.trim().is_empty() {
        "(unprintable)".to_owned()
    } else {
        kept
    }
}

/// A key as it is stored: the DER a PEM carried, with the armour gone.
///
/// The armour is where PKCS#1 and PKCS#8 were told apart, and it is not stored, so both are
/// tried and only bytes neither accepts are refused.
pub fn key_from_der(der: &[u8]) -> Result<SigningKey, DkimError> {
    RsaKeyPair::from_der(der)
        .or_else(|pkcs1| {
            RsaKeyPair::from_pkcs8(der).map_err(|pkcs8| DkimError::UnusableKey {
                pkcs1: pkcs1.to_string(),
                pkcs8: pkcs8.to_string(),
            })
        })
        .map(SigningKey)
}

/// The base64 `p=` value for the published record, and what
/// [`dns::check`](crate::sending_domains::dns::check) compares a published record against. The
/// bare value: `expected_p` is not the whole record, and handing it one fails every comparison.
pub fn public_key_p(key: &SigningKey) -> String {
    STANDARD.encode(pkcs1_to_spki(key.0.public().as_ref()))
}

/// The TXT value to publish at `{selector}._domainkey.{domain}`.
pub fn public_key_txt(key: &SigningKey) -> String {
    format!("v=DKIM1; k=rsa; p={}", public_key_p(key))
}

/// `AlgorithmIdentifier { OID 1.2.840.113549.1.1.1 (rsaEncryption), NULL }`, DER, which is the
/// fixed middle of every RSA SubjectPublicKeyInfo.
const RSA_ALGORITHM_ID: &[u8] = &[
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

/// `ring` hands out the PKCS#1 `RSAPublicKey`; a DKIM `p=` carries the SubjectPublicKeyInfo
/// around it: `SEQUENCE { AlgorithmIdentifier, BIT STRING { RSAPublicKey } }`.
pub fn pkcs1_to_spki(pkcs1: &[u8]) -> Vec<u8> {
    // The leading zero is the BIT STRING's count of unused bits in its last byte, of which a
    // whole number of bytes has none.
    let mut bits = vec![0u8];
    bits.extend_from_slice(pkcs1);
    let mut body = RSA_ALGORITHM_ID.to_vec();
    body.extend_from_slice(&element(0x03, &bits));
    element(0x30, &body)
}

/// The inverse: the PKCS#1 `RSAPublicKey` inside a SubjectPublicKeyInfo, which is the form
/// `ring`'s verification wants a published `p=` in.
pub fn spki_to_pkcs1(spki: &[u8]) -> Result<Vec<u8>, DkimError> {
    let body = contents(spki, 0x30, "it is not one DER SEQUENCE")?;
    let rest = body
        .strip_prefix(RSA_ALGORITHM_ID)
        .ok_or(DkimError::NotSpki("its algorithm is not rsaEncryption"))?;
    match contents(rest, 0x03, "it does not end with one key BIT STRING")?.split_first() {
        Some((0, pkcs1)) => Ok(pkcs1.to_vec()),
        _ => Err(DkimError::NotSpki(
            "its key BIT STRING is not a whole number of bytes",
        )),
    }
}

/// One DER element: tag, length (long form past 127 bytes), contents.
fn element(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if contents.len() < 0x80 {
        out.push(contents.len() as u8);
    } else {
        let len = contents.len().to_be_bytes();
        let len = &len[len.iter().position(|b| *b != 0).unwrap_or(len.len() - 1)..];
        out.push(0x80 | len.len() as u8);
        out.extend_from_slice(len);
    }
    out.extend_from_slice(contents);
    out
}

/// The contents of the DER element `input` is: one element carrying `tag`, and nothing after it.
/// Bytes past the element are refused rather than ignored — a key with something appended is not
/// the key, and silently taking the prefix would let two different inputs be the same key.
fn contents<'a>(input: &'a [u8], tag: u8, what: &'static str) -> Result<&'a [u8], DkimError> {
    let bad = || DkimError::NotSpki(what);
    let [found, first, rest @ ..] = input else {
        return Err(bad());
    };
    if *found != tag {
        return Err(bad());
    }
    let (len, rest) = if *first < 0x80 {
        (*first as usize, rest)
    } else {
        let n = (*first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return Err(bad());
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | *b as usize);
        (len, &rest[n..])
    };
    if rest.len() != len {
        return Err(bad());
    }
    Ok(rest)
}

// ---------------------------------------------------------------------------------------------
// Canonicalization (RFC 6376 §3.4.2 and §3.4.4).

/// Relaxed canonicalization of one header field, as it stands on the wire, folds and all. The
/// result carries the trailing CRLF the signing input wants.
fn relaxed_header(field: &[u8]) -> Vec<u8> {
    // A single field's only line breaks are its folds and its terminator, so dropping every CR
    // and LF unfolds it and takes the terminator off in one pass.
    let unfolded: Vec<u8> = field
        .iter()
        .copied()
        .filter(|b| !matches!(b, b'\r' | b'\n'))
        .collect();
    let colon = unfolded.iter().position(|b| *b == b':');
    let (name, value) = colon.map_or((&unfolded[..], &[][..]), |i| {
        (&unfolded[..i], &unfolded[i + 1..])
    });
    let mut out = name.trim_ascii().to_ascii_lowercase();
    out.push(b':');
    // `collapse` leaves one SP where a run was; §3.4.2 deletes the one after the colon too.
    out.extend_from_slice(collapse(value).trim_ascii_start());
    out.extend_from_slice(b"\r\n");
    out
}

/// Relaxed canonicalization of the message body.
fn relaxed_body(body: &[u8]) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = body
        .split(|b| *b == b'\n')
        .map(|line| collapse(line.strip_suffix(b"\r").unwrap_or(line)))
        .collect();
    // §3.4.4(b): ignore every empty line at the end. An empty or whitespace-only body therefore
    // canonicalizes to nothing at all. (The sentence about "a null input" belongs to §3.4.3, the
    // simple algorithm, which is the one that leaves a CRLF; the vectors are what settle this.)
    while lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    let mut out = Vec::with_capacity(body.len());
    for line in lines {
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Every run of WSP reduced to one SP, and the run at the end of the line dropped.
fn collapse(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut pending = false;
    for &b in line {
        if matches!(b, b' ' | b'\t') {
            pending = true;
        } else {
            if pending {
                out.push(b' ');
                pending = false;
            }
            out.push(b);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Signing.

/// The message with a `DKIM-Signature` header prepended: what goes on the wire.
pub fn signed_bytes(
    msg: &Message,
    key: &SigningKey,
    domain: &str,
    selector: &str,
) -> Result<Vec<u8>, DkimError> {
    // Both reach a mail header. `sending_domains` holds them to hostname characters already, so
    // this is the signer refusing to be the hole if some later caller does not.
    for (what, value) in [("domain", domain), ("selector", selector)] {
        if value.is_empty()
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(DkimError::BadName(what));
        }
    }

    let wire = msg.formatted();
    let at = find(&wire, b"\r\n\r\n").ok_or(DkimError::NoHeaderBreak)?;
    let (head, body) = (&wire[..at + 2], &wire[at + 4..]);

    let (signed, h) = signed_fields(&fields(head));

    let bh = STANDARD.encode(digest::digest(&digest::SHA256, &relaxed_body(body)));
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let tags = format!(
        "v=1; a=rsa-sha256; c=relaxed/relaxed; d={domain}; s={selector}; t={t}; h={}; bh={bh}; b=",
        h.join(":")
    );

    let mut input = Vec::new();
    for field in &signed {
        input.extend_from_slice(&relaxed_header(field));
    }
    // The signature's own field is signed last, with `b=` still empty and without a terminator.
    // What is signed is the canonicalization of the field as it will go out, folds and all, so
    // the verifier — which canonicalizes what it receives — is given the same bytes whatever
    // the folding does.
    let field = fold(&tags);
    let mut own = relaxed_header(field.as_bytes());
    own.truncate(own.len() - 2);
    input.extend_from_slice(&own);

    let mut b = vec![0u8; key.0.public().modulus_len()];
    key.0
        .sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            &input,
            &mut b,
        )
        .map_err(|_| DkimError::Sign)?;

    // The signature goes into the `b=` the signed field ends with, folded onto its own lines.
    // Nothing after `b=` is signed — a verifier empties the tag before it hashes the field — so
    // appending it here cannot disagree with what was signed above.
    let mut out = field.trim_end_matches("\r\n").to_owned();
    for piece in STANDARD.encode(&b).as_bytes().chunks(60) {
        out.push_str("\r\n\t");
        out.push_str(&String::from_utf8_lossy(piece));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(&wire);
    Ok(out)
}

/// The `DKIM-Signature` field as it will go out: one tag per line where a line would otherwise
/// run long. A tag is never split, because splitting one puts whitespace inside its value, and
/// `h=` is the tag long enough to tempt it.
fn fold(tags: &str) -> String {
    let mut out = String::from("DKIM-Signature:");
    let mut width = out.len();
    for word in tags.split(' ') {
        if width + 1 + word.len() > 78 {
            out.push_str("\r\n\t");
            width = 1;
        } else {
            out.push(' ');
            width += 1;
        }
        out.push_str(word);
        width += word.len();
    }
    out.push_str("\r\n");
    out
}

/// The fields to sign and the `h=` that names them: [`SIGNED_HEADERS`] restricted to the fields
/// the message carries, in that order.
///
/// RFC 6376 §5.4.2: a name that occurs more than once is signed at its **last** instance, so a
/// header slipped in above the real one is not what gets signed. No name is listed twice, so no
/// instance can be claimed twice either.
fn signed_fields<'a>(present: &[&'a [u8]]) -> (Vec<&'a [u8]>, Vec<&'static str>) {
    let mut signed = Vec::with_capacity(SIGNED_HEADERS.len());
    let mut h = Vec::with_capacity(SIGNED_HEADERS.len());
    for name in SIGNED_HEADERS {
        if let Some(field) = present
            .iter()
            .rev()
            .find(|f| field_name(f).eq_ignore_ascii_case(name.as_bytes()))
        {
            signed.push(*field);
            h.push(*name);
        }
    }
    (signed, h)
}

/// The first `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The header block's fields, in order, each with its folds and its CRLF.
fn fields(head: &[u8]) -> Vec<&[u8]> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    while start < head.len() {
        let end = find(&head[start..], b"\r\n").map_or(head.len(), |i| start + i + 2);
        if matches!(head[start], b' ' | b'\t') {
            // A continuation of the field above it, not a field of its own.
            if let Some(last) = spans.last_mut() {
                last.1 = end;
            }
        } else {
            spans.push((start, end));
        }
        start = end;
    }
    spans.into_iter().map(|(a, b)| &head[a..b]).collect()
}

/// The field's name, without the colon or the whitespace around it.
fn field_name(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|b| *b == b':').unwrap_or(field.len());
    field[..end].trim_ascii()
}

#[cfg(test)]
mod tests {
    use ring::signature::UnparsedPublicKey;

    use super::*;
    use crate::mail::{Outgoing, build};

    const SIGNING_PKCS8: &str = include_str!("../../tests/fixtures/dkim/signing-2048-pkcs8.pem");
    const SIGNING_PKCS1: &str = include_str!("../../tests/fixtures/dkim/signing-2048-pkcs1.pem");
    const TOO_SMALL: &str = include_str!("../../tests/fixtures/dkim/too-small-1024-pkcs1.pem");
    const NOT_RSA: &str = include_str!("../../tests/fixtures/dkim/not-rsa-ec.pem");
    const PUBLIC: &str = include_str!("../../tests/fixtures/dkim/public-2048.pem");

    fn key(pem: &str) -> SigningKey {
        key_from_pem(pem).expect("a test key")
    }

    fn message() -> Message {
        let unsubscribe_url: url::Url = "https://example.test/u/TOK".parse().unwrap();
        build(&Outgoing {
            from: "news@brand.test",
            from_name: Some("Brand"),
            to: "reader@dest.test",
            subject: "Hello",
            text: "plain body",
            html: Some("<p>html body</p>"),
            unsubscribe_url: &unsubscribe_url,
            feedback_id: "camp1:list1:nm",
            message_id_domain: "brand.test",
        })
        .expect("a message")
    }

    fn canon_header(field: &str) -> String {
        String::from_utf8(relaxed_header(field.as_bytes())).unwrap()
    }

    fn canon_body(body: &str) -> String {
        String::from_utf8(relaxed_body(body.as_bytes())).unwrap()
    }

    /// The `DKIM-Signature` field of `signed`, canonicalized and unfolded, without its name.
    fn signature_value(signed: &[u8]) -> String {
        let head = &signed[..find(signed, b"\r\n\r\n").expect("a header break") + 2];
        let field = fields(head)
            .into_iter()
            .find(|f| field_name(f).eq_ignore_ascii_case(b"DKIM-Signature"))
            .expect("a DKIM-Signature field");
        String::from_utf8(relaxed_header(field))
            .unwrap()
            .trim_end_matches("\r\n")
            .strip_prefix("dkim-signature:")
            .expect("the canonical name")
            .to_owned()
    }

    /// One tag of the signature, with the whitespace a fold left in it removed.
    fn tag(signed: &[u8], name: &str) -> String {
        signature_value(signed)
            .split(';')
            .find_map(|t| {
                let (k, v) = t.split_once('=')?;
                (k.trim() == name).then(|| v.chars().filter(|c| !c.is_whitespace()).collect())
            })
            .unwrap_or_else(|| panic!("no {name}= tag"))
    }

    /// A verifier's two steps (RFC 6376 §6.1.3): `bh=` must be the hash of the canonicalized
    /// body, and `b=` must be an RSA-SHA256 signature over the fields `h=` names plus this field
    /// with its own `b=` emptied. `p` is the published base64, as it would be read from DNS.
    fn verify(signed: &[u8], p: &str) -> bool {
        let at = find(signed, b"\r\n\r\n").expect("a header break");
        let (head, body) = (&signed[..at + 2], &signed[at + 4..]);
        let hashed = STANDARD.encode(digest::digest(&digest::SHA256, &relaxed_body(body)));
        if tag(signed, "bh") != hashed {
            return false;
        }
        let fields = fields(head);
        let mut input = Vec::new();
        for name in tag(signed, "h").split(':') {
            let field = fields
                .iter()
                .rev()
                .find(|f| field_name(f).eq_ignore_ascii_case(name.as_bytes()))
                .unwrap_or_else(|| panic!("h= names {name}, which the message does not carry"));
            input.extend_from_slice(&relaxed_header(field));
        }
        let emptied: Vec<String> = signature_value(signed)
            .split(';')
            .map(|t| {
                if t.trim_start().starts_with("b=") {
                    " b=".to_owned()
                } else {
                    t.to_owned()
                }
            })
            .collect();
        let mut field = relaxed_header(format!("dkim-signature:{}", emptied.join(";")).as_bytes());
        field.truncate(field.len() - 2);
        input.extend_from_slice(&field);

        let spki = STANDARD.decode(p).expect("p= is base64");
        let pkcs1 = spki_to_pkcs1(&spki).expect("p= is a SubjectPublicKeyInfo");
        let b = STANDARD.decode(tag(signed, "b")).expect("b= is base64");
        UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, pkcs1)
            .verify(&input, &b)
            .is_ok()
    }

    /// The `p=` out of the record an operator publishes, which is also what the DNS checker
    /// compares against — the two cannot drift, because this asserts they are the same string.
    fn published_p(key: &SigningKey) -> String {
        let txt = public_key_txt(key);
        let p = txt
            .strip_prefix("v=DKIM1; k=rsa; p=")
            .unwrap_or_else(|| panic!("the record is not a DKIM record: {txt}"))
            .to_owned();
        assert_eq!(p, public_key_p(key), "the record and `expected_p` disagree");
        p
    }

    /// RFC 6376's own canonicalization example, with its `<SP>`, `<HTAB>` and `<CRLF>` written
    /// out. The whole point of a published vector is to be used byte for byte.
    #[test]
    fn relaxed_header_canonicalization_matches_rfc_6376() {
        assert_eq!(canon_header("A: X\r\n"), "a:X\r\n");
        assert_eq!(canon_header("B : Y\t\r\n\tZ  \r\n"), "b:Y Z\r\n");
        // The signature's own field reaches the signing input without a terminator, and must
        // canonicalize the same way.
        assert_eq!(canon_header("B : Y\t\r\n\tZ  "), "b:Y Z\r\n");
        // A value that is only whitespace is empty, not a space.
        assert_eq!(canon_header("X: \t \r\n"), "x:\r\n");
    }

    #[test]
    fn relaxed_body_canonicalization_matches_rfc_6376() {
        assert_eq!(canon_body(" C \r\nD \t E\r\n\r\n\r\n"), " C\r\nD E\r\n");
        // Leading whitespace on a body line is reduced, not dropped: dropping it is the header
        // rule (§3.4.2's "delete WSP after the colon"), and using it here changes every hash.
        assert_eq!(canon_body("  C\r\n"), " C\r\n");
        // A body with no terminator gets one.
        assert_eq!(canon_body("hello"), "hello\r\n");
        // §3.4.4(b) ignores every empty line at the end, so a body of nothing but CRLFs is the
        // empty string and its `bh` is the SHA-256 of nothing — the empty-body `bh` every
        // implementation publishes, which is the check that settles it. Simple canonicalization
        // (§3.4.3) is the one that leaves a CRLF behind; this is not that.
        assert_eq!(canon_body(""), "");
        assert_eq!(canon_body("\r\n\r\n\r\n"), "");
        assert_eq!(
            STANDARD.encode(digest::digest(&digest::SHA256, &relaxed_body(b"\r\n\r\n"))),
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }

    /// RFC 8058 one-click needs the signature over both unsubscribe headers. If `h=` loses
    /// either, Gmail and Yahoo stop honouring one-click without saying so.
    #[test]
    fn h_covers_both_unsubscribe_headers() {
        let signed = signed_bytes(&message(), &key(SIGNING_PKCS8), "brand.test", "nm1").unwrap();
        let h = tag(&signed, "h");
        let names: Vec<String> = h.to_lowercase().split(':').map(str::to_owned).collect();
        for want in ["list-unsubscribe", "list-unsubscribe-post"] {
            assert!(names.contains(&want.to_owned()), "h={h}");
        }
        // Spelled out rather than derived from SIGNED_HEADERS, which would be that list
        // compared with itself: `From` is mandatory (RFC 6376 §5.4) and dropping it would pass
        // a test written that way.
        assert_eq!(
            names,
            [
                "from",
                "to",
                "subject",
                "date",
                "message-id",
                "mime-version",
                "content-type",
                "list-unsubscribe",
                "list-unsubscribe-post",
                "feedback-id",
            ],
            "h={h}"
        );
    }

    /// **The signature says which domain and which selector it was made for.** Swap `d=` and
    /// `s=` in the emitted tags and every receiver looks the key up at the selector's domain and
    /// the domain's selector: nothing is delivered, and nothing local notices.
    #[test]
    fn the_signature_names_the_domain_selector_and_algorithm_it_was_given() {
        let signed = signed_bytes(&message(), &key(SIGNING_PKCS8), "brand.test", "nm1").unwrap();
        assert_eq!(tag(&signed, "d"), "brand.test");
        assert_eq!(tag(&signed, "s"), "nm1");
        assert_eq!(tag(&signed, "v"), "1");
        assert_eq!(tag(&signed, "a"), "rsa-sha256");
        assert_eq!(tag(&signed, "c"), "relaxed/relaxed");
        // `t=` is when it was signed, in seconds since the epoch.
        let t: u64 = tag(&signed, "t").parse().expect("t= is a number");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(t <= now && now - t < 300, "t={t} against a clock at {now}");
    }

    /// RFC 6376 §5.4.2: where a name occurs more than once, the signature covers the last
    /// instance, so a header prepended above the real one is not the one that gets signed.
    #[test]
    fn a_repeated_header_is_signed_at_its_last_instance() {
        let head = b"From: forged@evil.test\r\nSubject: hi\r\nFrom: real@brand.test\r\n".as_slice();
        let (signed, h) = signed_fields(&fields(head));
        assert_eq!(h, ["From", "Subject"]);
        assert_eq!(
            String::from_utf8_lossy(signed[0]),
            "From: real@brand.test\r\n"
        );
        // A name the message does not carry is left out of `h=` rather than signed as empty.
        assert!(!h.contains(&"List-Unsubscribe"), "h={h:?}");
    }

    #[test]
    fn a_signature_verifies_against_the_published_public_key() {
        let key = key(SIGNING_PKCS8);
        let message = message();
        let signed = signed_bytes(&message, &key, "brand.test", "nm1").unwrap();
        let p = published_p(&key);
        assert!(verify(&signed, &p), "the signature does not verify");

        // What `p=` carries is a SubjectPublicKeyInfo. The PKCS#1 `RSAPublicKey` that `ring`
        // hands out has no algorithm identifier in it, so publishing that instead is visible
        // here and not only in a receiver's logs.
        let der = STANDARD.decode(&p).unwrap();
        assert!(
            der.windows(RSA_ALGORITHM_ID.len())
                .any(|w| w == RSA_ALGORITHM_ID),
            "p= is not a SubjectPublicKeyInfo"
        );

        // One byte of the body flipped. Without this half, the test above proves only that
        // signing produced some bytes.
        let mut tampered = signed.clone();
        let body_at = find(&signed, b"\r\n\r\n").unwrap() + 4;
        let letter = signed[body_at..]
            .iter()
            .position(u8::is_ascii_alphabetic)
            .expect("a letter in the body");
        tampered[body_at + letter] ^= 0x01;
        assert!(
            !verify(&tampered, &p),
            "a message whose body was edited still verified"
        );

        // And a signed header rewritten: the body hash still matches, and the signature does not.
        let rewritten = String::from_utf8(signed.clone())
            .unwrap()
            .replace("https://example.test/u/TOK", "https://evil.test/u/TOK");
        assert_ne!(rewritten.as_bytes(), &signed[..], "nothing was rewritten");
        assert!(
            !verify(rewritten.as_bytes(), &p),
            "a message whose unsubscribe link was rewritten still verified"
        );
    }

    #[test]
    fn spki_round_trips() {
        for pem in [SIGNING_PKCS8, SIGNING_PKCS1] {
            let key = key(pem);
            let pkcs1 = key.0.public().as_ref().to_vec();
            assert_eq!(spki_to_pkcs1(&pkcs1_to_spki(&pkcs1)).unwrap(), pkcs1);
        }
        // And what is not a SubjectPublicKeyInfo is refused rather than returned as one. The
        // last of these is a whole, valid key with four bytes stuck on the end: taking the
        // prefix and ignoring them would make two different inputs the same key.
        let pkcs1 = key(SIGNING_PKCS8).0.public().as_ref().to_vec();
        let trailing = [&pkcs1_to_spki(&pkcs1)[..], b"junk"].concat();
        for bad in [
            &b""[..],
            &[0x30, 0x03, 0x02, 0x01, 0x00][..],
            &pkcs1[..],
            &trailing[..],
        ] {
            assert!(
                matches!(spki_to_pkcs1(bad), Err(DkimError::NotSpki(_))),
                "accepted {} bytes as a public key",
                bad.len()
            );
        }
    }

    /// The stored key is DER with the armour stripped, and the armour is where PKCS#1 and PKCS#8
    /// were told apart. Both must load from the bytes alone.
    #[test]
    fn a_key_loads_from_either_armour_and_from_the_der_alone() {
        for pem in [SIGNING_PKCS8, SIGNING_PKCS1] {
            let from_pem = key(pem);
            let der = STANDARD
                .decode(
                    pem.trim()
                        .lines()
                        .filter(|l| !l.starts_with("-----"))
                        .map(str::trim)
                        .collect::<String>(),
                )
                .unwrap();
            let from_der = key_from_der(&der).expect("the stored form of the same key");
            assert_eq!(public_key_txt(&from_der), public_key_txt(&from_pem));
        }
        // The two fixtures are different keys, so the assertion above is not trivially true.
        assert_ne!(
            public_key_txt(&key(SIGNING_PKCS8)),
            public_key_txt(&key(SIGNING_PKCS1))
        );
    }

    #[test]
    fn key_from_pem_refuses_what_is_not_a_private_key() {
        let truncated = {
            let mut lines: Vec<String> = SIGNING_PKCS8.trim().lines().map(str::to_owned).collect();
            lines[1].pop();
            lines.join("\n")
        };
        let cases = [
            ("a public key", PUBLIC),
            ("an EC key", NOT_RSA),
            ("a 1024-bit key", TOO_SMALL),
            ("truncated base64", truncated.as_str()),
            ("nothing at all", ""),
            ("a PEM with no END", "-----BEGIN PRIVATE KEY-----\nAAAA\n"),
        ];
        let messages: Vec<(&str, String)> = cases
            .iter()
            .map(|(what, pem)| {
                let e = key_from_pem(pem)
                    .err()
                    .unwrap_or_else(|| panic!("{what} was accepted as a signing key"));
                (*what, e.to_string())
            })
            .collect();
        let says = |what: &str, want: &str| {
            let (_, m) = messages.iter().find(|(w, _)| *w == what).unwrap();
            assert!(
                m.to_lowercase().contains(&want.to_lowercase()),
                "{what}: {m:?} does not name {want:?}"
            );
        };
        says("a public key", "PUBLIC KEY");
        says("an EC key", "EC PRIVATE KEY");
        says("a 1024-bit key", "small");
        says("truncated base64", "base64");
        says("nothing at all", "PEM");
        says("a PEM with no END", "PEM");
        // The label is quoted back, so only the characters a PEM label is made of are: what is
        // in a refusal came from whoever sent the key.
        let Err(junk) = key_from_pem(
            "-----BEGIN <script>x</script>-----\nAAAA\n-----END <script>x</script>-----",
        ) else {
            panic!("a PEM labelled with anything at all was accepted");
        };
        let junk = junk.to_string();
        assert!(!junk.contains("script") && !junk.contains('<'), "{junk}");
        // No two of the first four read alike, so an operator is told which mistake they made.
        for (i, (what, a)) in messages.iter().enumerate().take(4) {
            for (other, b) in &messages[i + 1..4] {
                assert_ne!(a, b, "{what} and {other} fail alike");
            }
        }
    }

    /// `domain` and `selector` reach a mail header. The API holds them to hostname characters
    /// already; this is the signer refusing to be the hole if a future caller does not.
    #[test]
    fn a_domain_or_selector_that_could_forge_a_header_is_refused() {
        let key = key(SIGNING_PKCS8);
        let message = message();
        for (domain, selector) in [
            ("brand.test\r\nBcc: victim@dest.test", "nm1"),
            ("brand.test", "nm1\r\nBcc: victim@dest.test"),
            ("brand.test", ""),
            ("brand test", "nm1"),
        ] {
            assert!(
                matches!(
                    signed_bytes(&message, &key, domain, selector),
                    Err(DkimError::BadName(_))
                ),
                "{domain:?} / {selector:?} was signed"
            );
        }
    }

    #[test]
    fn the_signature_is_prepended_and_the_message_left_whole() {
        let message = message();
        let signed = signed_bytes(&message, &key(SIGNING_PKCS8), "brand.test", "nm1").unwrap();
        assert!(
            signed.starts_with(b"DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;"),
            "{}",
            String::from_utf8_lossy(&signed[..80.min(signed.len())])
        );
        assert!(
            signed.ends_with(&message.formatted()),
            "the message itself was not left alone"
        );
        // Folded: 78 bytes where a tag fits, and never over RFC 5322's hard 998, which is the
        // length at which a line stops being a line a relay will pass through unaltered. `h=`
        // is one unsplittable tag of 113 bytes, because splitting a tag puts whitespace inside
        // its value.
        let head = &signed[..find(&signed, b"\r\n\r\n").unwrap()];
        for line in head.split(|b| *b == b'\n') {
            assert!(
                line.len() <= 998,
                "a {} byte header line: {}",
                line.len(),
                String::from_utf8_lossy(line)
            );
        }
        assert!(
            String::from_utf8_lossy(head).contains("b=\r\n\t")
                && signature_value(&signed).contains("; b= "),
            "the signature itself was not folded onto its own lines: {}",
            String::from_utf8_lossy(head)
        );
    }
}
