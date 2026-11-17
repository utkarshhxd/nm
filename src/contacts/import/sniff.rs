// SPDX-License-Identifier: AGPL-3.0-only

//! What the server makes of a file's head: its delimiter, whether the first row is a header, and
//! what each column probably is.
//!
//! **A proposal that is not certain is blank.** A blank costs the operator one dropdown; a
//! confident wrong one writes a column into the wrong field, and the mapping screen is the last
//! place anyone would look for the cause. So a name is matched exactly (case aside) or not at all,
//! and a target two columns would claim goes to neither.

use std::collections::HashMap;

use super::{ColumnTarget, ContactField, csv::unescape_cell};
use crate::custom_fields::Definition;

/// Rows read to sniff a file; the rest of it is not looked at.
pub const SNIFF_ROWS: usize = 100;

/// Sample values kept per column.
const SAMPLES: usize = 5;

/// A sample is cut to this many characters: a cell can be as large as the head.
const SAMPLE_CHARS: usize = 100;

/// In preference order: on a tie the earlier one wins.
const DELIMITERS: [u8; 4] = [b',', b';', b'\t', b'|'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniffedColumn {
    pub name: String,
    /// The first non-blank values, as the operator's spreadsheet shows them.
    pub samples: Vec<String>,
    pub proposed: ColumnTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sniffed {
    pub delimiter: u8,
    pub has_header: bool,
    pub columns: Vec<SniffedColumn>,
}

fn records(head: &str, delimiter: u8) -> Vec<Vec<String>> {
    csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(head.as_bytes())
        .records()
        // A malformed record ends the sniff: what came before it is still a fair sample.
        .map_while(Result::ok)
        .take(SNIFF_ROWS)
        .map(|r| r.iter().map(str::to_owned).collect())
        .collect()
}

/// The delimiter under which the most rows have the same, more than one, number of fields; the
/// wider reading wins a tie, then the earlier delimiter. A file with no delimiter at all reads as
/// one comma-separated column.
fn detect(head: &str) -> (u8, Vec<Vec<String>>) {
    let mut best: Option<(usize, usize, u8, Vec<Vec<String>>)> = None;
    for d in DELIMITERS {
        let rows = records(head, d);
        let mut by_width: HashMap<usize, usize> = HashMap::new();
        for r in &rows {
            *by_width.entry(r.len()).or_default() += 1;
        }
        let Some((&width, &agree)) = by_width
            .iter()
            .filter(|(w, _)| **w > 1)
            .max_by_key(|(w, n)| (**n, **w))
        else {
            continue;
        };
        if best.as_ref().is_none_or(|b| (agree, width) > (b.0, b.1)) {
            best = Some((agree, width, d, rows));
        }
    }
    match best {
        Some((_, _, d, rows)) => (d, rows),
        None => (b',', records(head, b',')),
    }
}

/// `x@y.z`, with nothing cleverer: the only job is to tell a data row from a header row.
fn looks_like_email(cell: &str) -> bool {
    cell.split_once('@')
        .is_some_and(|(l, r)| !l.is_empty() && r.contains('.') && !cell.contains(' '))
}

/// The target a header names, or `Ignore` when it names none or more than one. Matching is on the
/// whole header, ignoring case: a built-in's wire name, a live custom field's key or label, or the
/// exporter's own `attr:<key>`. Archived fields take no new values, so they are never proposed.
fn propose(header: &str, defs: &[Definition]) -> ColumnTarget {
    let h = header.trim().to_lowercase();
    let mut hits: Vec<ColumnTarget> = ContactField::ALL
        .into_iter()
        .filter(|f| f.header() == h)
        .map(ColumnTarget::Field)
        .collect();
    let attr_key = h.strip_prefix("attr:");
    for d in defs.iter().filter(|d| d.archived_at.is_none()) {
        if d.key == h || d.label.trim().to_lowercase() == h || attr_key == Some(d.key.as_str()) {
            hits.push(ColumnTarget::Custom(d.key.clone()));
        }
    }
    match hits.as_slice() {
        [one] => one.clone(),
        _ => ColumnTarget::Ignore,
    }
}

/// `head` is the start of the file, with at most [`SNIFF_ROWS`] rows of it read. Writes nothing.
pub fn sniff(head: &str, defs: &[Definition]) -> Sniffed {
    let (delimiter, rows) = detect(head.strip_prefix('\u{feff}').unwrap_or(head));
    let Some(first) = rows.first() else {
        return Sniffed {
            delimiter,
            has_header: false,
            columns: vec![],
        };
    };
    // A header unless the first row holds an address: an address is data. Unsure means header,
    // the common case, and the operator can see the first row in the samples either way.
    let has_header = !first.iter().any(|c| looks_like_email(c));
    let mut columns: Vec<SniffedColumn> = (0..first.len())
        .map(|i| SniffedColumn {
            name: if has_header {
                unescape_cell(&first[i])
            } else {
                format!("Column {}", i + 1)
            },
            samples: rows
                .iter()
                .skip(usize::from(has_header))
                .filter_map(|r| r.get(i))
                .filter(|c| !c.trim().is_empty())
                .take(SAMPLES)
                .map(|c| unescape_cell(c).chars().take(SAMPLE_CHARS).collect())
                .collect(),
            proposed: if has_header {
                propose(&unescape_cell(&first[i]), defs)
            } else {
                ColumnTarget::Ignore
            },
        })
        .collect();
    // Two columns claiming one target means neither is certain.
    let mut claims: HashMap<ColumnTarget, usize> = HashMap::new();
    for c in &columns {
        *claims.entry(c.proposed.clone()).or_default() += 1;
    }
    for c in &mut columns {
        if c.proposed != ColumnTarget::Ignore && claims[&c.proposed] > 1 {
            c.proposed = ColumnTarget::Ignore;
        }
    }
    Sniffed {
        delimiter,
        has_header,
        columns,
    }
}
