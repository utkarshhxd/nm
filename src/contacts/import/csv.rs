// SPDX-License-Identifier: AGPL-3.0-only

//! What the importer undoes from a file the exporter wrote; `super::super::export::push_cell` and
//! `escape_in_rust` are the specification, and `tests/import_csv.rs` round-trips against them.

/// A cell with the exporter's formula-neutralising `'` removed. Exactly one, and only the first:
/// the exporter also prefixes a cell that already begins with `'`, so `''x` is the text `'x`.
pub fn unescape_cell(cell: &str) -> String {
    cell.strip_prefix('\'').unwrap_or(cell).to_owned()
}

/// `s` split on each `sep` that no backslash escapes, the pieces left **escaped**. A backslash
/// consumes the character after it, so `\\;` is an escaped backslash and then a real `;`.
fn split_unescaped(s: &str, sep: char) -> Vec<&str> {
    let (mut parts, mut start, mut escaped) = (Vec::new(), 0, false);
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == sep {
            parts.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    parts.push(&s[start..]);
    parts
}

/// `\\`, `\;` and `\:` turned back into `\`, `;` and `:`. Any other backslash is kept as written.
fn unescape(s: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(s.len()), s.chars());
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(next @ ('\\' | ';' | ':'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The entries of a `lists` cell, still escaped: a list entry's `:` has to survive until
/// [`split_list_entry`] has found the one that is not escaped. An empty cell has no entries.
pub fn split_multi_raw(cell: &str) -> Vec<&str> {
    if cell.is_empty() {
        return Vec::new();
    }
    split_unescaped(cell, ';')
}

/// The values of a `tags` cell or a multi_select `attr:<key>` cell, unescaped.
pub fn split_multi(cell: &str) -> Vec<String> {
    split_multi_raw(cell).into_iter().map(unescape).collect()
}

/// One `lists` entry (from [`split_multi_raw`]) as the list's name and its status. The status is
/// `None` when the entry has no unescaped `:`.
pub fn split_list_entry(entry: &str) -> (String, Option<String>) {
    let name = split_unescaped(entry, ':')[0];
    match entry.get(name.len() + 1..) {
        Some(status) => (unescape(name), Some(status.to_owned())),
        None => (unescape(name), None),
    }
}
