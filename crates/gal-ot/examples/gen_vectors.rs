//! Emit cross-language conformance vectors for the JavaScript OT engine.
//!
//! The browser client transforms the same ops the server does. If the two
//! implementations ever disagree — on tie-breaking, attribute handling, or
//! surrogate pairs — documents silently corrupt. This generates randomised
//! cases with the Rust results attached, and `web/ot.test.js` asserts the
//! JavaScript engine reproduces them exactly.
//!
//! Run: `cargo run -p gal-ot --example gen_vectors > vectors.json`

use gal_ot::*;
use serde_json::json;

/// Deterministic xorshift64*, so regenerating produces identical vectors.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Character-start offsets, so generated ops never split a surrogate pair.
fn boundaries(doc: &Delta) -> Vec<usize> {
    let mut offsets = vec![0usize];
    let mut offset = 0usize;
    for ch in doc.to_plain_text().chars() {
        offset += ch.len_utf16();
        offsets.push(offset);
    }
    offsets
}

fn random_delta(rng: &mut Rng, bounds: &[usize]) -> Delta {
    let last = bounds.len() - 1;
    let mut delta = Delta::new();
    let mut at = 0usize;

    while at < last {
        let end = at + 1 + rng.below(last - at);
        let span = bounds[end] - bounds[at];
        match rng.below(5) {
            0 => {
                delta.push(Op::retain(span));
                at = end;
            }
            1 => {
                delta.push(Op::delete(span));
                at = end;
            }
            2 => {
                let words = ["cat", "🌊", "xyz", "the ", "é", "日本"];
                delta.push(Op::insert(words[rng.below(words.len())]));
            }
            3 => {
                // An embed: one unit of the document however much JSON it
                // carries, and the only insert whose payload is not a string.
                // Both engines have to slice, merge and transform around it
                // identically — an attachment is exactly this op, so a
                // disagreement here corrupts a real document.
                let id = rng.below(3);
                delta.push(Op::embed(json!({
                    "attachment": { "id": format!("a-{id}"), "name": "plan.png" }
                })));
            }
            _ => {
                let keys = ["bold", "italic", "link"];
                let key = keys[rng.below(keys.len())];
                let value = match rng.below(3) {
                    0 => serde_json::Value::Null,
                    1 => json!(true),
                    _ => json!("https://example.com"),
                };
                delta.push(Op::retain(span).with_attr(key, value));
                at = end;
            }
        }
        if rng.below(8) == 0 {
            break;
        }
    }
    delta.chop()
}

fn main() {
    let mut rng = Rng(0x5DEE_CE66_D1CE_4B9F);
    let docs = [
        Delta::document("Hello world"),
        Delta::document("The quick brown fox jumps over the lazy dog"),
        Delta::document("a🌊b🌊c émoji 日本語"),
        Delta::new()
            .insert("mixed ")
            .insert_with(
                "formatting",
                [("bold".to_string(), json!(true))].into_iter().collect(),
            )
            .insert(" here"),
        // A document that already contains embeds, so ops are generated
        // *across* them rather than only appending them.
        Delta::new()
            .insert("see ")
            .embed(json!({ "attachment": { "id": "a-1", "name": "plan.png" } }))
            .insert(" and ")
            .embed(json!({ "attachment": { "id": "a-2", "name": "notes.txt" } }))
            .insert(" below"),
    ];

    let mut cases = Vec::new();
    for _ in 0..1500 {
        let doc = &docs[rng.below(docs.len())];
        let bounds = boundaries(doc);
        let a = random_delta(&mut rng, &bounds);
        let b = random_delta(&mut rng, &bounds);
        let position = rng.below(doc.len() + 1);

        cases.push(json!({
            "doc": doc,
            "a": a,
            "b": b,
            "position": position,
            "expected": {
                "composeDocA": compose(doc, &a),
                "composeAB": compose(&a, &b),
                "transformABTrue": transform(&a, &b, true),
                "transformBAFalse": transform(&b, &a, false),
                "invertA": invert(&a, doc),
                "positionTrue": transform_position(&a, position, true),
                "positionFalse": transform_position(&a, position, false),
            }
        }));
    }

    // Fixed cases covering the tie-breaking and edge behaviour that random
    // generation is unlikely to hit reliably.
    let fixed: Vec<(Delta, Delta, Delta)> = vec![
        (
            Delta::document("ab"),
            Delta::new().insert("X"),
            Delta::new().insert("Y"),
        ),
        (
            Delta::document("ab"),
            Delta::new().delete(2),
            Delta::new().delete(2),
        ),
        (
            Delta::document("ab"),
            Delta::new().retain(2).insert("Z"),
            Delta::new().delete(2),
        ),
        (
            Delta::new(),
            Delta::new().insert("first"),
            Delta::new().insert("second"),
        ),
        (
            Delta::document("🌊🌊"),
            Delta::new().retain(2).delete(2),
            Delta::new().delete(2).insert("x"),
        ),
    ];
    for (doc, a, b) in fixed {
        cases.push(json!({
            "doc": doc,
            "a": a,
            "b": b,
            "position": 1usize,
            "expected": {
                "composeDocA": compose(&doc, &a),
                "composeAB": compose(&a, &b),
                "transformABTrue": transform(&a, &b, true),
                "transformBAFalse": transform(&b, &a, false),
                "invertA": invert(&a, &doc),
                "positionTrue": transform_position(&a, 1, true),
                "positionFalse": transform_position(&a, 1, false),
            }
        }));
    }

    println!("{}", serde_json::to_string(&cases).unwrap());
}
