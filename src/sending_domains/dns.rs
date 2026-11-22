// SPDX-License-Identifier: AGPL-3.0-only

//! The DNS checker: whether the records a domain needs in order to send mail well are published.
//! SPF, DKIM, DMARC and (when one is configured) the return-path CNAME.
//!
//! The checks split in two. The *judging* — what a set of TXT records means — is plain functions
//! over strings, so what each check asserts is tested without a network. The *looking up* is
//! [`check_within`], which runs the four lookups concurrently, each under its own timeout: this
//! runs on an authenticated request, and a settings page must never wait on a nameserver that
//! does not answer.
//!
//! **A failure reason never carries key material.** A published DKIM `p=` that does not match the
//! expected one fails with a message that says so and prints neither value.

use std::time::Duration;

use hickory_resolver::{
    TokioResolver,
    config::{CLOUDFLARE, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
    proto::rr::{RData, RecordType},
    system_conf::read_system_conf,
};

use super::{CheckResult, DnsChecks, SendingDomain};

pub type Resolver = TokioResolver;

/// How long one lookup may take, retries and all. It is this module's bound, not the resolver's:
/// the resolver's own timeout is per attempt, per nameserver, and its product with the retries is
/// not a number anyone chose.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The resolver's per-attempt timeout. Two attempts fit inside [`LOOKUP_TIMEOUT`], so an answer
/// that is only slow is still waited for, and one that never comes is cut off by that bound.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// A resolver over `config`, built the way the checker needs it.
///
/// **It does not cache.** An operator fixes a record and presses "check" again within the
/// minute; a cache would answer with the record they just replaced, or with the `NXDOMAIN` they
/// just fixed, until its TTL ran out. The route is rate-limited, so every check going upstream is
/// affordable.
pub fn resolver(config: ResolverConfig, attempt_timeout: Duration) -> anyhow::Result<Resolver> {
    let mut builder = Resolver::builder_with_config(config, TokioRuntimeProvider::default());
    let options = builder.options_mut();
    options.timeout = attempt_timeout;
    options.cache_size = 0;
    Ok(builder.build()?)
}

/// The resolver the app holds: the host's own nameservers. A host whose resolver configuration
/// cannot be read falls back to Cloudflare's rather than refusing to start over a settings-page
/// helper.
pub fn system_resolver() -> anyhow::Result<Resolver> {
    let config = match read_system_conf() {
        Ok((config, _)) => config,
        Err(e) => {
            // The names looked up are customers' domains, and this sends them to a third party.
            // Falling back beats refusing to start; doing it silently would not be.
            tracing::warn!(
                error = %e,
                fallback = "Cloudflare (1.1.1.1)",
                "cannot read the system resolver configuration, so the DNS checker will send                  its lookups (your customers' sending domains) to Cloudflare's public resolver;                  fix the host's resolver configuration to keep them on your own infrastructure"
            );
            ResolverConfig::udp_and_tcp(&CLOUDFLARE)
        }
    };
    resolver(config, ATTEMPT_TIMEOUT)
}

/// Runs the four checks for `d` with [`LOOKUP_TIMEOUT`] per lookup.
///
/// `expected_p` is the base64 value a published `p=` must equal (the bare value, not the whole
/// TXT record), or `None` to accept any key.
pub async fn check(resolver: &Resolver, d: &SendingDomain, expected_p: Option<&str>) -> DnsChecks {
    check_within(resolver, d, expected_p, LOOKUP_TIMEOUT).await
}

/// [`check`] with the per-lookup bound given. The lookups run under `tokio::join!`, so the whole
/// takes about one `timeout` when every nameserver is silent, not four.
async fn check_within(
    resolver: &Resolver,
    d: &SendingDomain,
    expected_p: Option<&str>,
    timeout: Duration,
) -> DnsChecks {
    let dkim_name = format!("{}._domainkey.{}", d.dkim_selector, d.domain);
    let dmarc_name = format!("_dmarc.{}", d.domain);
    let (spf_txt, dkim_txt, dmarc_txt, return_path) = tokio::join!(
        txt(resolver, &d.domain, timeout),
        txt(resolver, &dkim_name, timeout),
        txt(resolver, &dmarc_name, timeout),
        async {
            let Some(rp) = d.return_path_domain.as_deref() else {
                return CheckResult::NotChecked;
            };
            // Alignment is a fact about two names, so a misaligned domain costs no lookup.
            if !aligned(&d.domain, rp) {
                return CheckResult::Fail(
                    "the return-path domain is neither the sending domain nor a subdomain of \
                     it, so DMARC cannot align it"
                        .to_owned(),
                );
            }
            match lookup(resolver, rp, RecordType::CNAME, timeout).await {
                Ok(found) if found.iter().any(|r| matches!(r, RData::CNAME(_))) => {
                    CheckResult::Pass
                }
                Ok(_) => CheckResult::Fail(
                    "no CNAME record is published at the return-path domain".to_owned(),
                ),
                Err(reason) => CheckResult::Fail(reason),
            }
        },
    );
    DnsChecks {
        spf: judge(spf_txt, spf),
        dkim: judge(dkim_txt, |r| dkim(r, expected_p)),
        dmarc: judge(dmarc_txt, dmarc),
        return_path,
    }
}

fn judge(
    found: Result<Vec<String>, String>,
    f: impl FnOnce(&[String]) -> CheckResult,
) -> CheckResult {
    match found {
        Ok(records) => f(&records),
        Err(reason) => CheckResult::Fail(reason),
    }
}

/// The TXT records at `name`, each with its character-strings joined (SPF and DKIM both say to
/// concatenate them without a separator). No such record is `Ok(vec![])`; an `Err` is a reason
/// fit to show an operator.
async fn txt(resolver: &Resolver, name: &str, timeout: Duration) -> Result<Vec<String>, String> {
    Ok(lookup(resolver, name, RecordType::TXT, timeout)
        .await?
        .into_iter()
        .filter_map(|r| match r {
            RData::TXT(t) => Some(
                t.txt_data
                    .iter()
                    .map(|s| String::from_utf8_lossy(s))
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect())
}

/// One lookup under `timeout`. The name is made absolute, so the host's search list can never
/// turn `example.com` into `example.com.corp.internal`.
async fn lookup(
    resolver: &Resolver,
    name: &str,
    record_type: RecordType,
    timeout: Duration,
) -> Result<Vec<RData>, String> {
    match tokio::time::timeout(timeout, resolver.lookup(format!("{name}."), record_type)).await {
        Err(_) => Err("the DNS lookup timed out".to_owned()),
        Ok(Err(e)) if e.is_no_records_found() => Ok(Vec::new()),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, name, "DNS lookup failed");
            Err("the DNS lookup failed".to_owned())
        }
        Ok(Ok(found)) => Ok(found.answers().iter().map(|r| r.data.clone()).collect()),
    }
}

/// Whether `record` is a `version` record: that text, any case, then the end or a separator.
fn is_version(record: &str, version: &str) -> bool {
    let record = record.trim_start();
    record
        .get(..version.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(version))
        && record[version.len()..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == ';')
}

/// Passes when exactly one of `records` is a `version` record.
fn exactly_one(records: &[String], version: &str, none: &str, several: &str) -> CheckResult {
    match records.iter().filter(|r| is_version(r, version)).count() {
        1 => CheckResult::Pass,
        0 => CheckResult::Fail(none.to_owned()),
        _ => CheckResult::Fail(several.to_owned()),
    }
}

/// SPF: a `v=spf1` record, and only one. Two is not belt and braces: RFC 7208 §4.5 makes it a
/// permanent error, so SPF fails for the whole domain.
fn spf(records: &[String]) -> CheckResult {
    exactly_one(
        records,
        "v=spf1",
        "no SPF record (v=spf1) is published at the domain",
        "more than one SPF record is published at the domain; two SPF records break SPF \
         entirely, so merge them into one",
    )
}

/// DMARC: a `v=DMARC1` record at `_dmarc.{domain}`, and only one: a receiver that finds several
/// ignores them all (RFC 7489 §6.6.3).
fn dmarc(records: &[String]) -> CheckResult {
    exactly_one(
        records,
        "v=DMARC1",
        "no DMARC record (v=DMARC1) is published at _dmarc",
        "more than one DMARC record is published at _dmarc; receivers ignore all of them, so \
         keep exactly one",
    )
}

/// DKIM: one `v=DKIM1` record with a non-empty `p=`, equal to `expected_p` when that is given.
/// Whitespace inside `p=` is not part of the key (RFC 6376 §3.6.1 allows folding there), so it is
/// dropped from both sides before they are compared.
fn dkim(records: &[String], expected_p: Option<&str>) -> CheckResult {
    let keys: Vec<&String> = records
        .iter()
        .filter(|r| tag(r, "v").is_some_and(|v| v.eq_ignore_ascii_case("DKIM1")))
        .collect();
    let record = match keys.as_slice() {
        [one] => one,
        [] if records.is_empty() => {
            return CheckResult::Fail("no TXT record is published at the DKIM selector".to_owned());
        }
        [] => {
            return CheckResult::Fail(
                "the TXT record at the DKIM selector does not start with v=DKIM1".to_owned(),
            );
        }
        _ => {
            return CheckResult::Fail(
                "more than one DKIM record is published at the selector; keep exactly one"
                    .to_owned(),
            );
        }
    };
    let Some(published) = tag(record, "p") else {
        return CheckResult::Fail("the DKIM record has no p= tag".to_owned());
    };
    if published.is_empty() {
        return CheckResult::Fail(
            "the DKIM record's p= is empty, which revokes the key".to_owned(),
        );
    }
    match expected_p {
        Some(expected) if strip_whitespace(expected) != published => CheckResult::Fail(
            "the published p= does not match the key this domain signs with".to_owned(),
        ),
        _ => CheckResult::Pass,
    }
}

/// The value of tag `name` in a `tag=value; tag=value` record, whitespace removed, or `None`.
fn tag(record: &str, name: &str) -> Option<String> {
    record.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| strip_whitespace(v))
    })
}

fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Relaxed alignment between the sending domain and the return-path domain: equal, or one a
/// subdomain of the other.
///
/// shortcut: no public-suffix list, so "organizational domain" is whichever of the two names is
/// the shorter. That is exact for a return path that is a subdomain of the sending domain, which
/// is the shape every provider's instructions give; add the list if a from-domain that is itself
/// a subdomain of one it does not own ever needs telling apart.
fn aligned(domain: &str, return_path: &str) -> bool {
    let (a, b) = (
        domain.trim_end_matches('.').to_ascii_lowercase(),
        return_path.trim_end_matches('.').to_ascii_lowercase(),
    );
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use hickory_resolver::config::NameServerConfig;
    use uuid::Uuid;

    use super::*;
    use crate::sending_domains::Signing;

    fn txts(records: &[&str]) -> Vec<String> {
        records.iter().map(|r| (*r).to_owned()).collect()
    }

    fn reason(r: CheckResult) -> String {
        match r {
            CheckResult::Fail(reason) => reason,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn one_spf_record_passes_and_none_fails() {
        assert_eq!(
            spf(&txts(&["google-site-verification=abc", "v=spf1 -all"])),
            CheckResult::Pass
        );
        // Case and leading space are not a different record; a lookalike prefix is.
        assert_eq!(
            spf(&txts(&["  V=SPF1 include:a.example"])),
            CheckResult::Pass
        );
        assert!(matches!(
            spf(&txts(&["v=spf10 -all"])),
            CheckResult::Fail(_)
        ));
        assert!(matches!(spf(&[]), CheckResult::Fail(_)));
    }

    #[test]
    fn two_spf_records_fail_with_a_reason_naming_the_duplication() {
        let r = reason(spf(&txts(&[
            "v=spf1 include:a.example -all",
            "v=spf1 -all",
        ])));
        assert!(r.contains("more than one SPF"), "{r}");
        // A different reason from "none": the operator must be told which mistake they made.
        assert_ne!(r, reason(spf(&[])));
    }

    #[test]
    fn dmarc_needs_exactly_one_record() {
        assert_eq!(dmarc(&txts(&["v=DMARC1; p=none"])), CheckResult::Pass);
        assert_eq!(dmarc(&txts(&["v=DMARC1"])), CheckResult::Pass);
        assert!(matches!(
            dmarc(&txts(&["v=spf1 -all"])),
            CheckResult::Fail(_)
        ));
        assert!(
            reason(dmarc(&txts(&["v=DMARC1; p=none", "v=DMARC1; p=reject"])))
                .contains("more than one")
        );
    }

    #[test]
    fn dkim_needs_v_and_a_non_empty_p() {
        assert_eq!(
            dkim(&txts(&["v=DKIM1; k=rsa; p=QUJD"]), None),
            CheckResult::Pass
        );
        assert!(reason(dkim(&txts(&["v=DKIM1; k=rsa"]), None)).contains("no p= tag"));
        assert!(reason(dkim(&txts(&["v=DKIM1; k=rsa; p="]), None)).contains("empty"));
        assert!(reason(dkim(&txts(&["k=rsa; p=QUJD"]), None)).contains("v=DKIM1"));
        assert!(reason(dkim(&[], None)).contains("no TXT record"));
        assert!(
            reason(dkim(&txts(&["v=DKIM1; p=QUJD", "v=DKIM1; p=REVG"]), None))
                .contains("more than one")
        );
    }

    #[test]
    fn a_published_p_that_differs_fails_without_printing_either_key() {
        let published = "PUBLISHEDKEYBASE64+abc/123=";
        let expected = "EXPECTEDKEYBASE64+xyz/789=";
        let record = format!("v=DKIM1; k=rsa; p={published}");
        let r = reason(dkim(&txts(&[&record]), Some(expected)));
        assert!(r.contains("does not match"), "{r}");
        // Not even a fragment of either: a reason is stored and shown, and keys are not for that.
        for secret in [published, expected] {
            for window in secret.as_bytes().windows(6) {
                let w = std::str::from_utf8(window).unwrap();
                assert!(!r.contains(w), "reason leaks {w:?}: {r}");
            }
        }
        // The same record against the key it publishes passes, so the failure above is the
        // comparison and not a record the parser refuses.
        assert_eq!(dkim(&txts(&[&record]), Some(published)), CheckResult::Pass);
    }

    #[test]
    fn p_is_compared_without_the_whitespace_a_record_may_fold_into_it() {
        let record = "v=DKIM1; k=rsa; p=QUJD REVG\tR0hJ";
        assert_eq!(
            dkim(&txts(&[record]), Some("QUJDREVGR0hJ")),
            CheckResult::Pass
        );
        assert_eq!(
            dkim(&txts(&[record]), Some("QUJD REVG R0hJ")),
            CheckResult::Pass
        );
    }

    #[test]
    fn relaxed_alignment_accepts_a_subdomain_and_rejects_a_stranger() {
        assert!(aligned("example.com", "example.com"));
        assert!(aligned("example.com", "bounce.mail.example.com"));
        assert!(aligned("bounce.example.com", "example.com"));
        assert!(aligned("Example.com", "BOUNCE.example.COM."));
        assert!(!aligned("example.com", "example.net"));
        // A suffix of the characters is not a parent domain.
        assert!(!aligned("example.com", "notexample.com"));
    }

    fn domain(return_path: Option<&str>) -> SendingDomain {
        SendingDomain {
            id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            domain: "example.com".to_owned(),
            from_name: None,
            signing: Signing::Relay,
            dkim_selector: "nm1".to_owned(),
            return_path_domain: return_path.map(str::to_owned),
            checks: DnsChecks::default(),
            checked_at: None,
            send_unverified_acknowledged_at: None,
        }
    }

    /// A resolver aimed at a UDP socket that is open and never answers: the nameserver that
    /// hangs, without depending on what a routable black-holed address does on this network. The
    /// resolver's own timeout is 30 s and it retries, so only `check_within`'s bound can end a
    /// lookup in under a second.
    fn mute_nameserver() -> (std::net::UdpSocket, Resolver) {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut ns = NameServerConfig::udp("127.0.0.1".parse().unwrap());
        ns.connections[0].port = socket.local_addr().unwrap().port();
        let config = ResolverConfig::from_name_servers(vec![ns]);
        (socket, resolver(config, Duration::from_secs(30)).unwrap())
    }

    #[tokio::test]
    async fn a_lookup_that_times_out_is_a_recorded_failure() {
        let (_socket, resolver) = mute_nameserver();
        let timeout = Duration::from_millis(750);

        // One lookup alone: a Fail that says why, and not before its timeout. `tokio::time::timeout`
        // never fires early, so the lower bound holds on any machine; it is what tells a lookup
        // that waited from one that failed at once for some other reason.
        let started = Instant::now();
        let one = txt(&resolver, "example.com", timeout).await;
        let waited = started.elapsed();
        assert_eq!(one, Err("the DNS lookup timed out".to_owned()));
        assert!(
            waited >= timeout,
            "gave up after {waited:?}, before its {timeout:?}"
        );
        assert!(
            waited < timeout * 2,
            "took {waited:?} for a {timeout:?} bound"
        );

        // The whole check, with all four lookups outstanding: every verdict is a recorded
        // failure, and the total is under twice one timeout. Run one after another the four
        // would take four times it, so the upper bound is also what proves they overlap. Both
        // bounds are inequalities on purpose: the elapsed time is timeout plus whatever the
        // scheduler adds, and an equality would be correct only on an idle machine.
        let started = Instant::now();
        let checks = check_within(
            &resolver,
            &domain(Some("bounce.example.com")),
            None,
            timeout,
        )
        .await;
        let total = started.elapsed();
        for (what, result) in [
            ("spf", &checks.spf),
            ("dkim", &checks.dkim),
            ("dmarc", &checks.dmarc),
            ("return_path", &checks.return_path),
        ] {
            assert!(
                matches!(result, CheckResult::Fail(r) if r.contains("timed out")),
                "{what}: {result:?}"
            );
        }
        assert!(
            total >= timeout,
            "returned in {total:?}, before one {timeout:?}"
        );
        assert!(
            total < timeout * 2,
            "four lookups took {total:?}; serial would be 4x {timeout:?}"
        );
    }

    #[tokio::test]
    async fn a_misaligned_return_path_fails_without_a_lookup() {
        let (socket, resolver) = mute_nameserver();
        socket.set_nonblocking(true).unwrap();
        let checks = check_within(
            &resolver,
            &domain(Some("bounce.example.net")),
            None,
            Duration::from_millis(100),
        )
        .await;
        assert!(
            matches!(&checks.return_path, CheckResult::Fail(r) if r.contains("neither the sending domain")),
            "{:?}",
            checks.return_path
        );
        // The other three went to the (mute) nameserver and the misaligned one did not. Each
        // question carries its name uncompressed, so "bounce" would be in the datagram's bytes.
        let mut datagrams = Vec::new();
        let mut buf = [0u8; 512];
        while let Ok(n) = socket.recv(&mut buf) {
            datagrams.push(buf[..n].to_vec());
        }
        assert!(
            datagrams.len() >= 3,
            "expected the three TXT lookups: {}",
            datagrams.len()
        );
        assert!(
            datagrams
                .iter()
                .all(|d| !d.windows(6).any(|w| w == b"bounce")),
            "the misaligned return-path domain was looked up"
        );
    }
}
