// SPDX-License-Identifier: AGPL-3.0-only

//! The block tree: what the visual editor posts and `templates.tree` stores. The wire format is
//! `.superpowers/sdd/m2b/contract-block-tree.md`; this file implements it and does not extend it.

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

/// `deny_unknown_fields` on both enums: this is JSON from anything holding a session, and a field
/// a variant does not define is refused rather than quietly dropped. The editor rejects the same
/// shapes, but that is a convenience for the operator, not a boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Block {
    Text {
        runs: Vec<Run>,
    },
    Heading {
        #[serde(deserialize_with = "heading_level")]
        level: u8,
        runs: Vec<Run>,
    },
    Image {
        src: String,
        alt: String,
    },
    Button {
        label: String,
        href: String,
    },
    Divider {},
    Spacer {
        height: u16,
    },
    Columns {
        #[serde(deserialize_with = "no_nested_columns")]
        children: Vec<Vec<Block>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Run {
    Text {
        text: String,
        marks: Vec<Mark>,
    },
    Link {
        href: String,
        text: String,
        marks: Vec<Mark>,
    },
    /// The tag as the operator typed it, braces included. The compiled AST never crosses the wire.
    Tag {
        source: String,
    },
}

fn heading_level<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    let level = u8::deserialize(d)?;
    if (1..=6).contains(&level) {
        Ok(level)
    } else {
        Err(serde::de::Error::custom(format!(
            "heading level must be 1 to 6, got {level}"
        )))
    }
}

/// MJML cannot render a section inside a column, so `Columns` holds leaf blocks only.
fn no_nested_columns<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<Block>>, D::Error> {
    let children = Vec::<Vec<Block>>::deserialize(d)?;
    if children
        .iter()
        .flatten()
        .any(|b| matches!(b, Block::Columns { .. }))
    {
        return Err(serde::de::Error::custom(
            "Columns cannot nest inside Columns",
        ));
    }
    Ok(children)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mark {
    Bold,
    Italic,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> Result<Block, serde_json::Error> {
        serde_json::from_value(v)
    }

    fn refused(v: serde_json::Value) -> String {
        match parse(v) {
            Ok(b) => panic!("accepted a malformed block: {b:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn every_variant_round_trips_in_the_contracts_shape() {
        let wire = json!([
            {"type":"Text","runs":[
                {"type":"Text","text":"hi","marks":["bold","italic"]},
                {"type":"Link","href":"https://a.example/","text":"x","marks":[]},
                {"type":"Tag","source":"{{ first_name | \"there\" }}"}]},
            {"type":"Heading","level":2,"runs":[]},
            {"type":"Image","src":"https://a.example/i.png","alt":"a"},
            {"type":"Button","label":"Go","href":"https://a.example/"},
            {"type":"Divider"},
            {"type":"Spacer","height":24},
            {"type":"Columns","children":[[{"type":"Divider"}],[]]}
        ]);
        let blocks: Vec<Block> = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(blocks.len(), 7);
        assert_eq!(serde_json::to_value(&blocks).unwrap(), wire);
    }

    #[test]
    fn an_unknown_block_type_is_refused() {
        refused(json!({"type":"Video","src":"x"}));
        refused(json!({"type":"text","runs":[]}));
        refused(json!({"runs":[]}));
    }

    #[test]
    fn an_unknown_run_type_is_refused() {
        for run in [
            json!({"type":"Plain","text":"x"}),
            json!({"type":"Bold","text":"x"}),
            json!({"type":"Break"}),
        ] {
            refused(json!({"type":"Text","runs":[run]}));
        }
    }

    #[test]
    fn columns_inside_columns_is_refused() {
        let inner = json!({"type":"Columns","children":[[]]});
        refused(json!({"type":"Columns","children":[[inner.clone()]]}));
        // Not only as the first child, and not only one level down.
        refused(
            json!({"type":"Columns","children":[[{"type":"Divider"}],[{"type":"Divider"}, inner]]}),
        );
        // A sibling Columns is fine; only nesting is refused.
        parse(json!({"type":"Columns","children":[[{"type":"Divider"}]]})).unwrap();
    }

    #[test]
    fn an_unknown_mark_is_refused() {
        for mark in ["underline", "Bold", "strike", ""] {
            refused(json!({"type":"Text","runs":[{"type":"Text","text":"x","marks":[mark]}]}));
            refused(json!({"type":"Text","runs":[
                {"type":"Link","href":"h","text":"x","marks":[mark]}]}));
        }
    }

    #[test]
    fn extra_fields_are_refused_on_every_block_variant() {
        let extra = ("onclick", json!("alert(1)"));
        for mut v in [
            json!({"type":"Text","runs":[]}),
            json!({"type":"Heading","level":1,"runs":[]}),
            json!({"type":"Image","src":"s","alt":"a"}),
            json!({"type":"Button","label":"l","href":"h"}),
            json!({"type":"Divider"}),
            json!({"type":"Spacer","height":1}),
            json!({"type":"Columns","children":[]}),
        ] {
            v[extra.0] = extra.1.clone();
            let msg = refused(v.clone());
            assert!(msg.contains("onclick"), "{v}: {msg}");
        }
    }

    #[test]
    fn extra_fields_are_refused_on_every_run_variant() {
        for run in [
            json!({"type":"Text","text":"x","marks":[],"style":"x"}),
            json!({"type":"Link","href":"h","text":"x","marks":[],"target":"_top"}),
            json!({"type":"Tag","source":"{{ a }}","raw":true}),
        ] {
            refused(json!({"type":"Text","runs":[run]}));
        }
    }

    #[test]
    fn a_missing_field_is_refused_rather_than_defaulted() {
        refused(json!({"type":"Text","runs":[{"type":"Text","text":"x"}]}));
        refused(json!({"type":"Image","src":"s"}));
    }

    #[test]
    fn a_heading_level_outside_one_to_six_is_refused() {
        for level in [0, 7, 255] {
            refused(json!({"type":"Heading","level":level,"runs":[]}));
        }
        for level in 1..=6 {
            parse(json!({"type":"Heading","level":level,"runs":[]})).unwrap();
        }
    }
}
