// SPDX-License-Identifier: AGPL-3.0-only

//! The merge-tag language (spec §5.3): a closed grammar, parsed once at save into an AST. The AST
//! is the only thing stored; the operator's source text is never parsed again at send time.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::custom_fields::Definition;

/// How deep blocks may nest. Enforced at compile, so no renderer ever walks a deeper tree.
pub const MAX_DEPTH: usize = 32;

/// The contact columns a tag may name, alongside the workspace's custom fields.
const SYSTEM_FIELDS: [&str; 4] = ["email", "first_name", "last_name", "timezone"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    Text(String),
    Field {
        key: String,
        filter: Option<Filter>,
        fallback: Option<String>,
    },
    Present {
        key: String,
        body: Vec<Node>,
    },
    Absent {
        key: String,
        body: Vec<Node>,
    },
    Equals {
        key: String,
        value: String,
        body: Vec<Node>,
    },
}

/// Frozen at these four. Adding one is a spec change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    Upper,
    Lower,
    Title,
    Date(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagError {
    UnknownField(String),
    ArchivedField(String),
    UnknownFilter(String),
    /// `{{ x | upper | "a" }}`. The grammar allows a filter or a fallback, never both; saying so
    /// beats `UnknownFilter("\"a\"")`, which sends the operator hunting for a filter they never
    /// meant to write.
    FilterAndFallback,
    NonLiteralFallback,
    Unbalanced(String),
    TooDeep,
    Malformed(String),
}

/// The names a tag may use: system fields plus this workspace's custom fields. Archived fields are
/// kept apart so a new tag naming one is told why, rather than "unknown".
pub struct Registry {
    live: HashSet<String>,
    archived: HashSet<String>,
}

impl Registry {
    pub fn from_definitions(defs: &[Definition]) -> Self {
        let mut live: HashSet<String> = SYSTEM_FIELDS.iter().map(|s| s.to_string()).collect();
        let mut archived = HashSet::new();
        for d in defs {
            // The unsubscribe slot is the product's alone (spec §4.5 ruling 6), and a custom field
            // may be keyed like anything (issues/44). Admitting this one would hand an operator tag
            // the recipient's token, so it is never a name an operator registry knows.
            if d.key == super::UNSUBSCRIBE_TOKEN {
                continue;
            }
            if d.archived_at.is_some() {
                archived.insert(d.key.clone());
            } else {
                live.insert(d.key.clone());
            }
        }
        // A custom field may be keyed like a system field (`validate_key` does not reserve the
        // names — issues/44), so `live` and `archived` can hold the same key. Nothing is pruned
        // here: `check` asks `live` first, which is what makes the system field win. A
        // `retain(|k| !live.contains(k))` used to sit here and could not change any answer, so it
        // read as a guard while being dead; the lookup ORDER is the guard, and the test names it.
        Self { live, archived }
    }

    /// The registry the system footer is compiled against: it knows
    /// [`UNSUBSCRIBE_TOKEN`](super::UNSUBSCRIBE_TOKEN) and nothing else, and it is the only one that
    /// knows that.
    pub fn system_footer() -> Self {
        Self {
            live: HashSet::from([super::UNSUBSCRIBE_TOKEN.to_owned()]),
            archived: HashSet::new(),
        }
    }

    /// Let `keys` be named although they are archived.
    ///
    /// A template compiled before an archive still renders, which is R2's rule — but a RE-SAVE of
    /// that same document has to be possible too, or the archive quietly makes the template
    /// unopenable the next time anything else moves its fingerprint: the preview says "save it
    /// again" and the save refuses the tag the stored version already had. The operator is then
    /// locked out of their own template with no way back.
    ///
    /// A key that is archived and NOT in `keys` is still refused. That is a *new* reference to an
    /// archived field, which is what the rule is actually about. Only a key this registry already
    /// knows to be archived is moved, so an unknown key stays unknown and a caller cannot widen
    /// the namespace with this.
    pub fn allowing<'a>(mut self, keys: impl IntoIterator<Item = &'a str>) -> Self {
        for key in keys {
            if self.archived.remove(key) {
                self.live.insert(key.to_owned());
            }
        }
        self
    }

    fn check(&self, key: &str) -> Result<(), TagError> {
        if self.live.contains(key) {
            Ok(())
        } else if self.archived.contains(key) {
            Err(TagError::ArchivedField(key.to_string()))
        } else {
            Err(TagError::UnknownField(key.to_string()))
        }
    }
}

/// A block waiting for its `{{/ key }}`.
struct Open {
    key: String,
    kind: Kind,
    body: Vec<Node>,
}

enum Kind {
    Present,
    Absent,
    Equals(String),
}

/// The depth of an AST that was READ rather than parsed.
///
/// [`compile`] enforces [`MAX_DEPTH`] from the length of its own stack, so a tree it returns is
/// within the cap by construction. `serde`'s `Deserialize` on [`Node`] re-validates **nothing**:
/// a tree that came back out of `templates.compiled` was not parsed here and may have been
/// written by an older build, a future one, or anything that is not `compile`. Every renderer
/// recurses over the tree and assumes the cap, so anything that LOADS an AST calls this first.
///
/// Iterative rather than recursive, because the tree it is handed is exactly the one that must
/// not be walked by a recursion: a deep one has to be refused, not discovered by overflowing the
/// stack on the way to finding out.
pub fn check_depth(nodes: &[Node]) -> Result<(), TagError> {
    let mut pending: Vec<(&[Node], usize)> = vec![(nodes, 0)];
    while let Some((level, depth)) = pending.pop() {
        for node in level {
            let body = match node {
                Node::Text(_) | Node::Field { .. } => continue,
                Node::Present { body, .. }
                | Node::Absent { body, .. }
                | Node::Equals { body, .. } => body,
            };
            // `compile` refuses to open a block while `stack.len() >= MAX_DEPTH`, so a block at
            // nesting level `MAX_DEPTH` is the deepest it will produce. Same comparison here.
            if depth + 1 > MAX_DEPTH {
                return Err(TagError::TooDeep);
            }
            pending.push((body, depth + 1));
        }
    }
    Ok(())
}

/// One pass over `source`. Text between tags is copied verbatim and tags become nodes; nothing
/// the AST later produces is ever fed back in here.
pub fn compile(source: &str, registry: &Registry) -> Result<Vec<Node>, TagError> {
    let mut root: Vec<Node> = Vec::new();
    // The depth cap is this stack's length, so it cannot drift from the parse.
    let mut stack: Vec<Open> = Vec::new();
    let mut rest = source;

    while let Some(start) = rest.find("{{") {
        if start > 0 {
            push(&mut stack, &mut root, Node::Text(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        // shortcut: the first `}}` ends a tag, so a fallback literal cannot contain `}}`.
        let end = after
            .find("}}")
            .ok_or_else(|| TagError::Malformed("unterminated tag".into()))?;
        let inner = after[..end].trim();
        rest = &after[end + 2..];

        match inner.as_bytes().first() {
            Some(b'#') | Some(b'^') => {
                let body = inner[1..].trim();
                let (key, kind) = if inner.starts_with('#') {
                    open_tag(body)?
                } else {
                    (key_only(body)?, Kind::Absent)
                };
                registry.check(&key)?;
                if stack.len() >= MAX_DEPTH {
                    return Err(TagError::TooDeep);
                }
                stack.push(Open {
                    key,
                    kind,
                    body: Vec::new(),
                });
            }
            Some(b'/') => {
                let key = key_only(inner[1..].trim())?;
                match stack.pop() {
                    Some(open) if open.key == key => {
                        let body = open.body;
                        let node = match open.kind {
                            Kind::Present => Node::Present { key, body },
                            Kind::Absent => Node::Absent { key, body },
                            Kind::Equals(value) => Node::Equals { key, value, body },
                        };
                        push(&mut stack, &mut root, node);
                    }
                    _ => return Err(TagError::Unbalanced(key)),
                }
            }
            _ => {
                let node = field_tag(inner, registry)?;
                push(&mut stack, &mut root, node);
            }
        }
    }
    if !rest.is_empty() {
        push(&mut stack, &mut root, Node::Text(rest.to_string()));
    }
    match stack.pop() {
        Some(open) => Err(TagError::Unbalanced(open.key)),
        None => Ok(root),
    }
}

fn push(stack: &mut [Open], root: &mut Vec<Node>, node: Node) {
    match stack.last_mut() {
        Some(open) => open.body.push(node),
        None => root.push(node),
    }
}

/// `^[a-z][a-z0-9_]{0,62}$`, the same shape `custom_fields::validate_key` enforces.
fn is_key(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn key_only(s: &str) -> Result<String, TagError> {
    if is_key(s) {
        Ok(s.to_string())
    } else {
        Err(TagError::Malformed(format!("not a field name: {s:?}")))
    }
}

/// `"text"` with no quote inside: the only thing a fallback or an equality may hold.
fn literal(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_string())
}

/// The inside of `{{# … }}`: `key` or `key = "literal"`.
fn open_tag(s: &str) -> Result<(String, Kind), TagError> {
    match s.split_once('=') {
        None => Ok((key_only(s)?, Kind::Present)),
        Some((key, value)) => {
            let value = literal(value.trim())
                .ok_or_else(|| TagError::Malformed("an equality needs a quoted literal".into()))?;
            Ok((key_only(key.trim())?, Kind::Equals(value)))
        }
    }
}

/// A strftime format for `date:`. The format runs to the end of the tag, so unchecked it swallows
/// whatever follows: `{{ x | date:%Y | "a" }}` would compile to `Date("%Y | \"a\"")` and the
/// operator's fallback would vanish into a strftime literal. The charset rejection catches that;
/// the sample format catches `%Q`, which is valid-looking text and would otherwise fail at send,
/// which is the render-time surprise compiling at save exists to prevent. Internal spaces stay
/// legal — `date:%d %B %Y` is a real format.
fn date_format(raw: &str) -> Result<String, TagError> {
    let fmt = raw.trim();
    let bad = |m: &str| Err(TagError::Malformed(format!("{m}: {fmt:?}")));
    if fmt.is_empty() {
        return bad("empty date format");
    }
    if fmt.contains(['|', '"', '{', '}']) {
        return bad("a date format may not contain | \" { or }");
    }
    let sample = chrono::DateTime::from_timestamp(946_782_245, 0).expect("a fixed valid timestamp");
    let mut out = String::new();
    if write!(out, "{}", sample.format(fmt)).is_err() {
        return bad("not a strftime format");
    }
    Ok(fmt.to_string())
}

/// The inside of `{{ … }}`: `key`, `key | "fallback"` or `key | filter`.
///
/// After the `|`, a quoted string is a fallback literal and a bare word is a filter position —
/// always, so an unknown bare word is `UnknownFilter` whatever the workspace's fields are called.
/// Deciding it by whether the word happens to name a field would make the same template yield
/// different errors in different workspaces, and turn the error into an oracle for which fields
/// exist.
fn field_tag(s: &str, registry: &Registry) -> Result<Node, TagError> {
    let (key, arg) = match s.split_once('|') {
        None => (s, None),
        Some((k, a)) => (k.trim(), Some(a.trim())),
    };
    let key = key_only(key)?;
    registry.check(&key)?;
    let (mut filter, mut fallback) = (None, None);
    if let Some(arg) = arg {
        match literal(arg) {
            // A fallback may hold a `|`, so the literal is tried before the second-`|` check.
            Some(text) => fallback = Some(text),
            None if arg.contains('|') => return Err(TagError::FilterAndFallback),
            None if arg.starts_with('"') => return Err(TagError::NonLiteralFallback),
            None => {
                filter = Some(match arg {
                    "upper" => Filter::Upper,
                    "lower" => Filter::Lower,
                    "title" => Filter::Title,
                    _ => match arg.strip_prefix("date:") {
                        Some(fmt) => Filter::Date(date_format(fmt)?),
                        None => return Err(TagError::UnknownFilter(arg.to_string())),
                    },
                });
            }
        }
    }
    Ok(Node::Field {
        key,
        filter,
        fallback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_fields::FieldKind;

    fn def(key: &str, archived: bool) -> Definition {
        Definition {
            id: uuid::Uuid::nil(),
            key: key.into(),
            label: key.into(),
            kind: FieldKind::Text,
            options: vec![],
            archived_at: archived.then(chrono::Utc::now),
        }
    }

    fn registry() -> Registry {
        Registry::from_definitions(&[def("country", false), def("old_plan", true)])
    }

    fn field(key: &str) -> Node {
        Node::Field {
            key: key.into(),
            filter: None,
            fallback: None,
        }
    }

    #[test]
    fn a_known_field_compiles() {
        let r = registry();
        assert_eq!(
            compile(
                "Hi {{ first_name | \"there\" }}, {{ country | upper }}!",
                &r
            )
            .unwrap(),
            vec![
                Node::Text("Hi ".into()),
                Node::Field {
                    key: "first_name".into(),
                    filter: None,
                    fallback: Some("there".into())
                },
                Node::Text(", ".into()),
                Node::Field {
                    key: "country".into(),
                    filter: Some(Filter::Upper),
                    fallback: None
                },
                Node::Text("!".into()),
            ]
        );
        assert_eq!(
            compile("{{email|date:%Y-%m-%d}}", &r).unwrap(),
            vec![Node::Field {
                key: "email".into(),
                filter: Some(Filter::Date("%Y-%m-%d".into())),
                fallback: None
            }]
        );
        assert_eq!(
            compile("no tags, a { and a }", &r).unwrap(),
            vec![Node::Text("no tags, a { and a }".into())]
        );
    }

    #[test]
    fn an_unknown_field_is_a_compile_error() {
        let r = registry();
        assert_eq!(
            compile("{{ nope }}", &r),
            Err(TagError::UnknownField("nope".into()))
        );
        // Blocks are checked too: a conditional is not a way around the registry.
        assert_eq!(
            compile("{{# nope }}x{{/ nope }}", &r),
            Err(TagError::UnknownField("nope".into()))
        );
    }

    /// `custom_fields::validate_key` does not reserve the system names (issues/44), so a
    /// workspace may define a custom field keyed `email`. Archiving it must not take `{{ email }}`
    /// away from every template in that workspace. What prevents that is `check` asking `live`
    /// before `archived`, and this is the test that holds the order there.
    #[test]
    fn a_system_field_survives_an_archived_custom_field_of_the_same_name() {
        let r = Registry::from_definitions(&[def("email", true), def("first_name", true)]);
        assert!(compile("{{ email }}", &r).is_ok());
        assert!(compile("{{# first_name }}x{{/ first_name }}", &r).is_ok());
    }

    #[test]
    fn an_archived_field_is_a_compile_error() {
        let r = registry();
        assert_eq!(
            compile("{{ old_plan }}", &r),
            Err(TagError::ArchivedField("old_plan".into()))
        );
        assert_eq!(
            compile("{{# old_plan }}x{{/ old_plan }}", &r),
            Err(TagError::ArchivedField("old_plan".into()))
        );
    }

    #[test]
    fn a_fallback_must_be_a_quoted_literal() {
        let r = registry();
        // After `|`, a bare word is a filter position whatever it is named — including a word that
        // happens to name a field. Deciding it by the registry would make the error depend on the
        // workspace and turn it into an oracle for which fields exist.
        assert_eq!(
            compile("{{ first_name | last_name }}", &r),
            Err(TagError::UnknownFilter("last_name".into()))
        );
        // A quote that does not close, or closes early.
        assert_eq!(
            compile("{{ first_name | \"there }}", &r),
            Err(TagError::NonLiteralFallback)
        );
        assert_eq!(
            compile("{{ first_name | \"a\" + \"b\" }}", &r),
            Err(TagError::NonLiteralFallback)
        );
        // Single quotes are not quotes.
        assert_eq!(
            compile("{{ first_name | 'there' }}", &r),
            Err(TagError::UnknownFilter("'there'".into()))
        );
        // A `|` inside the quotes belongs to the fallback: the tag splits on the FIRST `|`.
        assert_eq!(
            compile("{{ first_name | \"a|b\" }}", &r).unwrap(),
            vec![Node::Field {
                key: "first_name".into(),
                filter: None,
                fallback: Some("a|b".into())
            }]
        );
        // An equality takes a quoted literal too.
        assert!(matches!(
            compile("{{# country = US }}x{{/ country }}", &r),
            Err(TagError::Malformed(_))
        ));
    }

    #[test]
    fn a_filter_and_a_fallback_cannot_be_combined() {
        let r = registry();
        for t in [
            "{{ first_name | upper | \"a\" }}",
            "{{ first_name | \"a\" | upper }}",
            "{{ first_name | date:%Y | \"a\" }}",
        ] {
            assert_eq!(compile(t, &r), Err(TagError::FilterAndFallback), "{t}");
        }
    }

    /// The frozen list is the reason this language has no `env`, no `include` and no expressions,
    /// so it is pinned as an allowlist: all four compile to the right variant and anything else is
    /// refused. A denylist of names cannot say "and nothing else".
    #[test]
    fn the_filter_list_is_frozen_at_four() {
        let r = registry();
        let filter = |t: &str| match compile(t, &r) {
            Ok(nodes) => match nodes.as_slice() {
                [Node::Field { filter, .. }] => filter.clone(),
                other => panic!("{t}: {other:?}"),
            },
            Err(e) => panic!("{t}: {e:?}"),
        };
        assert_eq!(filter("{{ first_name | upper }}"), Some(Filter::Upper));
        assert_eq!(filter("{{ first_name | lower }}"), Some(Filter::Lower));
        assert_eq!(filter("{{ first_name | title }}"), Some(Filter::Title));
        assert_eq!(
            filter("{{ first_name | date:%d %B %Y }}"),
            Some(Filter::Date("%d %B %Y".into()))
        );
        assert_eq!(filter("{{ first_name }}"), None);
        for f in [
            "env",
            "include",
            "render",
            "trim",
            "capitalize",
            "default",
            "escape",
            "raw",
            "safe",
            "UPPER",
            "upper:x",
        ] {
            assert_eq!(
                compile(&format!("{{{{ first_name | {f} }}}}"), &r),
                Err(TagError::UnknownFilter(f.into())),
                "{f}"
            );
        }
    }

    /// `date:` runs to the end of the tag, so an unchecked format swallows whatever follows; and a
    /// format that is only wrong at send defeats the point of compiling at save.
    #[test]
    fn a_date_format_is_validated_at_compile() {
        let r = registry();
        for t in [
            "{{ first_name | date: }}",
            "{{ first_name | date:%\" }}",
            "{{ first_name | date:% }}",
            "{{ first_name | date:{{ email }} }}",
        ] {
            assert!(
                matches!(compile(t, &r), Err(TagError::Malformed(_))),
                "{t}: {:?}",
                compile(t, &r)
            );
        }
        // The format is trimmed, so `date: %Y` does not keep its leading space.
        assert_eq!(
            compile("{{ first_name | date: %Y-%m-%d }}", &r).unwrap(),
            vec![Node::Field {
                key: "first_name".into(),
                filter: Some(Filter::Date("%Y-%m-%d".into())),
                fallback: None
            }]
        );
    }

    /// The contact columns a tag may name without a custom field definition. Pinned, because
    /// dropping one breaks every template that uses it and no other test names them.
    #[test]
    fn the_system_field_list_is_frozen_at_four() {
        assert_eq!(
            SYSTEM_FIELDS,
            ["email", "first_name", "last_name", "timezone"]
        );
        let r = Registry::from_definitions(&[]);
        for f in SYSTEM_FIELDS {
            assert!(compile(&format!("{{{{ {f} }}}}"), &r).is_ok(), "{f}");
        }
    }

    #[test]
    fn a_key_must_be_lowercase_snake_case_within_63_characters() {
        assert!(is_key("a"));
        assert!(is_key("a_1"));
        assert!(is_key(&"a".repeat(63)));
        for bad in ["", "A", "1a", "a-b", "a b", "a.b", "_a", "aB"] {
            assert!(!is_key(bad), "{bad:?}");
        }
        assert!(!is_key(&"a".repeat(64)));
    }

    #[test]
    fn present_absent_and_equals_blocks_compile() {
        let nodes = compile(
            "{{# first_name }}Hi {{ first_name }}{{/ first_name }}\
             {{^ first_name }}Hi{{/ first_name }}\
             {{# country = \"US\" }}US{{/ country }}",
            &registry(),
        )
        .unwrap();
        assert_eq!(
            nodes,
            vec![
                Node::Present {
                    key: "first_name".into(),
                    body: vec![Node::Text("Hi ".into()), field("first_name")]
                },
                Node::Absent {
                    key: "first_name".into(),
                    body: vec![Node::Text("Hi".into())]
                },
                Node::Equals {
                    key: "country".into(),
                    value: "US".into(),
                    body: vec![Node::Text("US".into())]
                },
            ]
        );
    }

    #[test]
    fn an_unbalanced_block_is_a_compile_error() {
        let r = registry();
        assert_eq!(
            compile("{{# first_name }}x", &r),
            Err(TagError::Unbalanced("first_name".into()))
        );
        assert_eq!(
            compile("x{{/ first_name }}", &r),
            Err(TagError::Unbalanced("first_name".into()))
        );
        assert_eq!(
            compile(
                "{{# first_name }}{{# country }}{{/ first_name }}{{/ country }}",
                &r
            ),
            Err(TagError::Unbalanced("first_name".into()))
        );
        assert!(matches!(
            compile("{{ first_name", &r),
            Err(TagError::Malformed(_))
        ));
    }

    #[test]
    fn nesting_past_max_depth_is_a_compile_error() {
        let r = registry();
        let nest = |n: usize| {
            let mut s = "{{# first_name }}".repeat(n);
            s.push('x');
            s.push_str(&"{{/ first_name }}".repeat(n));
            s
        };
        assert_eq!(MAX_DEPTH, 32);
        assert!(compile(&nest(MAX_DEPTH), &r).is_ok());
        assert_eq!(compile(&nest(MAX_DEPTH + 1), &r), Err(TagError::TooDeep));
        assert_eq!(compile(&nest(10_000), &r), Err(TagError::TooDeep));
        // Unclosed too: the cap must not wait for the closers to arrive.
        assert_eq!(
            compile(&"{{^ first_name }}".repeat(10_000), &r),
            Err(TagError::TooDeep)
        );
    }
}
