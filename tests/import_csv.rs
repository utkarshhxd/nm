// SPDX-License-Identifier: AGPL-3.0-only

//! The importer's unescape functions, checked against the exporter that wrote the file. Every
//! expectation that matters is a round trip through the exporter's own escaper: a string written
//! out by hand would only encode this file's reading of the format.

use nm::contacts::{
    export::{attr_text, escape_in_rust, push_cell},
    import::csv::{split_list_entry, split_multi, split_multi_raw, unescape_cell},
};
use serde_json::json;

/// What the exporter writes for a single cell, then what a CSV reader hands back from it: the
/// surrounding quotes dropped and `""` read as `"`.
fn through_the_file(cell: &str) -> String {
    let mut out = String::new();
    push_cell(&mut out, cell);
    match out.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(quoted) => quoted.replace("\"\"", "\""),
        None => out,
    }
}

const AWKWARD: [&str; 12] = [
    "a;b",
    "a:b",
    r"a\b",
    r"a\;b",
    r"a\\;b",
    "'quoted",
    "''twice",
    "",
    "a\nb",
    "=SUM(1)",
    "-5",
    r#"say "hi", ok"#,
];

#[test]
fn a_leading_apostrophe_is_stripped_once() {
    assert_eq!(unescape_cell("'=cmd"), "=cmd");
    assert_eq!(unescape_cell("''=cmd"), "'=cmd");
}

#[test]
fn a_semicolon_inside_a_name_does_not_split_it() {
    assert_eq!(split_multi(r"a\;b;c"), vec!["a;b", "c"]);
}

#[test]
fn an_escaped_backslash_does_not_escape_the_semicolon_after_it() {
    assert_eq!(split_multi(r"a\\;b"), vec![r"a\", "b"]);
}

#[test]
fn a_list_entry_splits_at_its_one_unescaped_colon() {
    assert_eq!(
        split_list_entry(r"Ops\: EU:subscribed"),
        ("Ops: EU".into(), Some("subscribed".into()))
    );
    assert_eq!(split_list_entry("Ops"), ("Ops".into(), None));
}

#[test]
fn an_empty_cell_has_no_entries() {
    assert!(split_multi("").is_empty());
    assert!(split_multi_raw("").is_empty());
}

#[test]
fn every_escaped_cell_round_trips_through_the_exporter() {
    for original in AWKWARD {
        let read = unescape_cell(&through_the_file(original));
        assert_eq!(read, original, "{original:?}");
    }
}

#[test]
fn every_multi_select_option_round_trips_through_the_exporter() {
    let options: Vec<&str> = AWKWARD.into_iter().filter(|o| !o.is_empty()).collect();
    let cell = attr_text(Some(&json!(options)));
    let read = split_multi(&unescape_cell(&through_the_file(&cell)));
    assert_eq!(read, options);
}

#[test]
fn every_list_entry_round_trips_through_the_exporter() {
    let entries: Vec<String> = AWKWARD
        .into_iter()
        .filter(|n| !n.is_empty())
        .map(|name| format!("{}:unsubscribed", escape_in_rust(name)))
        .collect();
    let cell = unescape_cell(&through_the_file(&entries.join(";")));
    let raw = split_multi_raw(&cell);
    assert_eq!(raw.len(), AWKWARD.len() - 1);
    for (entry, name) in raw
        .into_iter()
        .zip(AWKWARD.into_iter().filter(|n| !n.is_empty()))
    {
        assert_eq!(
            split_list_entry(entry),
            (name.to_owned(), Some("unsubscribed".to_owned()))
        );
    }
}
