//! Rich-text attribute maps and their transformation rules.
//!
//! An attribute map carries formatting for a run of text (`bold`, `link`, …).
//! A `null` value is meaningful: it represents *removal* of an attribute, which
//! is what lets a retain op strip formatting.

use serde_json::Value;
use std::collections::BTreeMap;

/// Formatting applied to a run of text. `BTreeMap` keeps the JSON wire format
/// deterministic, which matters for hashing and for test assertions.
pub type Attributes = BTreeMap<String, Value>;

/// Compose `b` on top of `a`.
///
/// When `keep_null` is set, explicit removals survive into the result; that is
/// correct for retain-over-retain (the removal still needs to be applied to the
/// underlying document) but wrong for retain-over-insert, where the text being
/// inserted simply never had the attribute in the first place.
pub fn compose(a: &Attributes, b: &Attributes, keep_null: bool) -> Attributes {
    let mut out: Attributes = if keep_null {
        b.clone()
    } else {
        b.iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    for (key, value) in a {
        if !b.contains_key(key) {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}

/// Transform attribute map `b` against concurrent map `a`.
///
/// Only the higher-priority side's formatting survives a conflict; the loser
/// drops the keys the winner also set. Without `priority`, `b` passes through
/// untouched because it is the winner.
pub fn transform(a: &Attributes, b: &Attributes, priority: bool) -> Attributes {
    if !priority {
        return b.clone();
    }
    b.iter()
        .filter(|(key, _)| !a.contains_key(*key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Build the attribute map that undoes applying `attr` to a run formatted with
/// `base`.
pub fn invert(attr: &Attributes, base: &Attributes) -> Attributes {
    let mut out = Attributes::new();
    // Attributes the change overwrote: restore the original value.
    for (key, value) in base {
        if base.get(key) != attr.get(key) && attr.contains_key(key) {
            out.insert(key.clone(), value.clone());
        }
    }
    // Attributes the change introduced: remove them.
    for (key, value) in attr {
        if Some(value) != base.get(key) && !base.contains_key(key) {
            out.insert(key.clone(), Value::Null);
        }
    }
    out
}

/// Strip `null` entries, yielding the attributes as they appear on a document.
pub fn without_nulls(attrs: &Attributes) -> Attributes {
    attrs
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(pairs: &[(&str, Value)]) -> Attributes {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn compose_merges_disjoint_keys() {
        let a = attrs(&[("bold", json!(true))]);
        let b = attrs(&[("italic", json!(true))]);
        assert_eq!(
            compose(&a, &b, false),
            attrs(&[("bold", json!(true)), ("italic", json!(true))])
        );
    }

    #[test]
    fn compose_lets_b_win_conflicts() {
        let a = attrs(&[("color", json!("red"))]);
        let b = attrs(&[("color", json!("blue"))]);
        assert_eq!(compose(&a, &b, false), attrs(&[("color", json!("blue"))]));
    }

    #[test]
    fn compose_null_handling_depends_on_keep_null() {
        let a = attrs(&[("bold", json!(true))]);
        let b = attrs(&[("bold", Value::Null)]);
        // Retain over insert: the inserted text never had the attribute.
        assert_eq!(compose(&a, &b, false), Attributes::new());
        // Retain over retain: the removal must reach the document.
        assert_eq!(compose(&a, &b, true), attrs(&[("bold", Value::Null)]));
    }

    #[test]
    fn transform_without_priority_is_identity() {
        let a = attrs(&[("bold", json!(true))]);
        let b = attrs(&[("bold", json!(false))]);
        assert_eq!(transform(&a, &b, false), b);
    }

    #[test]
    fn transform_with_priority_drops_conflicts() {
        let a = attrs(&[("bold", json!(true)), ("color", json!("red"))]);
        let b = attrs(&[("bold", json!(false)), ("italic", json!(true))]);
        assert_eq!(transform(&a, &b, true), attrs(&[("italic", json!(true))]));
    }

    #[test]
    fn invert_restores_overwritten_and_removes_added() {
        let base = attrs(&[("bold", json!(true))]);
        let attr = attrs(&[("bold", json!(false)), ("italic", json!(true))]);
        assert_eq!(
            invert(&attr, &base),
            attrs(&[("bold", json!(true)), ("italic", Value::Null)])
        );
    }

    #[test]
    fn invert_of_noop_is_empty() {
        let base = attrs(&[("bold", json!(true))]);
        assert_eq!(invert(&Attributes::new(), &base), Attributes::new());
    }

    #[test]
    fn invert_roundtrips_through_compose() {
        let base = attrs(&[("bold", json!(true)), ("color", json!("red"))]);
        let change = attrs(&[("bold", Value::Null), ("italic", json!(true))]);
        let applied = compose(&base, &change, false);
        let undo = invert(&change, &base);
        assert_eq!(compose(&applied, &undo, false), base);
    }
}
