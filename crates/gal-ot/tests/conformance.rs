//! Replay the frozen conformance vectors against the Rust engine.
//!
//! `tests/ot.test.js` replays the same file against the JavaScript engine, so
//! between them the two implementations are pinned to one immutable record of
//! what the algebra does.
//!
//! The pinning is the point. For a long time the vectors were regenerated from
//! the Rust engine immediately before being replayed, which meant they could
//! only ever detect JavaScript drifting away from Rust — a change to Rust's own
//! transform semantics rewrote its expectations on the way past and the suite
//! stayed green. Since both engines must agree *and* neither may quietly change
//! what it agrees on, the file is checked in and this test reads it.
//!
//! When a change to the algebra is deliberate, regenerate the file and commit
//! the diff as part of the same change, so the new behaviour is reviewed rather
//! than absorbed:
//!
//! ```sh
//! cargo run -p gal-ot --example gen_vectors > tests/vectors.json
//! ```

use gal_ot::{compose, invert, transform, transform_position, Delta};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    doc: Delta,
    a: Delta,
    b: Delta,
    position: usize,
    expected: Expected,
}

/// The field names are the JavaScript test's, so both engines read one file.
/// `rename_all` would give `composeAb` for `compose_ab`, hence the spelled-out
/// renames.
#[derive(Deserialize)]
struct Expected {
    #[serde(rename = "composeDocA")]
    compose_doc_a: Delta,
    #[serde(rename = "composeAB")]
    compose_ab: Delta,
    #[serde(rename = "transformABTrue")]
    transform_ab_true: Delta,
    #[serde(rename = "transformBAFalse")]
    transform_ba_false: Delta,
    #[serde(rename = "invertA")]
    invert_a: Delta,
    #[serde(rename = "positionTrue")]
    position_true: usize,
    #[serde(rename = "positionFalse")]
    position_false: usize,
}

fn vectors() -> Vec<Case> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors.json");
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read the conformance vectors at {path}: {e}\n\
             They are checked in; if the file is genuinely missing, regenerate it with\n\
             `cargo run -p gal-ot --example gen_vectors > tests/vectors.json` and review the diff."
        )
    });
    serde_json::from_str(&raw).expect("the conformance vectors are not valid JSON")
}

#[test]
fn the_rust_engine_reproduces_every_frozen_vector() {
    let cases = vectors();
    assert!(
        cases.len() > 1000,
        "expected the full vector set, found {} cases",
        cases.len()
    );

    for (i, case) in cases.iter().enumerate() {
        let want = &case.expected;
        // Reported one field at a time: a transform change usually breaks
        // several at once, and the first one named is the most specific.
        let checks: [(&str, Delta, &Delta); 5] = [
            (
                "compose(doc, a)",
                compose(&case.doc, &case.a),
                &want.compose_doc_a,
            ),
            ("compose(a, b)", compose(&case.a, &case.b), &want.compose_ab),
            (
                "transform(a, b, true)",
                transform(&case.a, &case.b, true),
                &want.transform_ab_true,
            ),
            (
                "transform(b, a, false)",
                transform(&case.b, &case.a, false),
                &want.transform_ba_false,
            ),
            ("invert(a, doc)", invert(&case.a, &case.doc), &want.invert_a),
        ];

        for (name, got, expected) in checks {
            assert_eq!(
                &got, expected,
                "case {i}: {name} no longer matches the frozen vector\n  \
                 doc: {:?}\n  a:   {:?}\n  b:   {:?}",
                case.doc, case.a, case.b
            );
        }

        assert_eq!(
            transform_position(&case.a, case.position, true),
            want.position_true,
            "case {i}: transform_position(a, {}, true) no longer matches",
            case.position
        );
        assert_eq!(
            transform_position(&case.a, case.position, false),
            want.position_false,
            "case {i}: transform_position(a, {}, false) no longer matches",
            case.position
        );
    }
}
