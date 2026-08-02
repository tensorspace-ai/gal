use super::*;
use serde_json::json;

fn attrs(pairs: &[(&str, serde_json::Value)]) -> Attributes {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn bold() -> Attributes {
    attrs(&[("bold", json!(true))])
}

fn text_of(d: &Delta) -> String {
    d.to_plain_text()
}

// --- basic construction -------------------------------------------------

#[test]
fn push_merges_adjacent_text() {
    let d = Delta::new().insert("Hello").insert(" world");
    assert_eq!(d.ops.len(), 1);
    assert_eq!(text_of(&d), "Hello world");
}

#[test]
fn push_does_not_merge_across_differing_attributes() {
    let d = Delta::new().insert("Hello").insert_with(" world", bold());
    assert_eq!(d.ops.len(), 2);
}

#[test]
fn push_merges_adjacent_deletes_and_retains() {
    let d = Delta::new().delete(2).delete(3);
    assert_eq!(d.ops.len(), 1);
    assert_eq!(d.ops[0].len(), 5);

    let d = Delta::new().retain(2).retain(3);
    assert_eq!(d.ops, vec![Op::retain(5)]);
}

#[test]
fn push_orders_insert_before_delete_at_same_position() {
    // Delete-then-insert and insert-then-delete are equivalent; canonicalising
    // the order keeps structurally identical deltas comparable.
    let a = Delta::new().retain(1).delete(2).insert("x");
    let b = Delta::new().retain(1).insert("x").delete(2);
    assert_eq!(a, b);
}

#[test]
fn zero_length_ops_are_dropped() {
    let d = Delta::new().insert("").retain(0).delete(0);
    assert!(d.is_empty());
}

#[test]
fn chop_removes_trailing_bare_retain() {
    let d = Delta::new().insert("hi").retain(4).chop();
    assert_eq!(d.ops, vec![Op::insert("hi")]);

    // A retain carrying attributes is meaningful and must survive.
    let d = Delta::new().insert("hi").retain_with(4, bold()).chop();
    assert_eq!(d.ops.len(), 2);
}

#[test]
fn lengths_are_measured_correctly() {
    let d = Delta::new().insert("abc").retain(2).delete(4);
    assert_eq!(d.len(), 9);
    assert_eq!(d.base_length(), 6);
    assert_eq!(d.target_length(), 5);
    assert_eq!(d.change_length(), -1);
}

// --- wire format --------------------------------------------------------

#[test]
fn json_round_trips_in_quill_format() {
    let d = Delta::new().retain(3).insert_with("hi", bold()).delete(2);
    let encoded = serde_json::to_string(&d).unwrap();
    assert_eq!(
        encoded,
        r#"{"ops":[{"retain":3},{"insert":"hi","attributes":{"bold":true}},{"delete":2}]}"#
    );
    assert_eq!(serde_json::from_str::<Delta>(&encoded).unwrap(), d);
}

#[test]
fn embeds_round_trip_and_occupy_one_unit() {
    let d = Delta::new().insert("a").embed(json!({"image": "cat.png"}));
    assert_eq!(d.len(), 2);
    let encoded = serde_json::to_string(&d).unwrap();
    assert_eq!(serde_json::from_str::<Delta>(&encoded).unwrap(), d);
}

/// `ops` defaults to empty, so a misspelled key parsed as an empty delta: the
/// server accepted the submit, applied nothing, and acked a revision. The
/// user's typing vanished and their client believed it had landed.
#[test]
fn a_delta_with_a_misspelled_key_is_rejected_rather_than_read_as_empty() {
    assert!(serde_json::from_str::<Delta>(r#"{"opz":[{"retain":3}]}"#).is_err());
    // Singular `attribute` silently dropped the formatting it carried.
    assert!(serde_json::from_str::<Delta>(r#"{"ops":[{"retain":3,"attribute":{}}]}"#).is_err());
    // An empty delta is still legitimate when it is what was actually sent.
    assert_eq!(
        serde_json::from_str::<Delta>(r#"{"ops":[]}"#).unwrap(),
        Delta::new()
    );
}

#[test]
fn malformed_ops_are_rejected() {
    assert!(serde_json::from_str::<Delta>(r#"{"ops":[{}]}"#).is_err());
    assert!(serde_json::from_str::<Delta>(r#"{"ops":[{"retain":1,"delete":1}]}"#).is_err());
}

// --- compose ------------------------------------------------------------

#[test]
fn compose_applies_insert_to_document() {
    let doc = Delta::document("Hello");
    let change = Delta::new().retain(5).insert(" world");
    assert_eq!(text_of(&doc.apply(&change)), "Hello world");
}

#[test]
fn compose_applies_delete_to_document() {
    let doc = Delta::document("Hello world");
    let change = Delta::new().retain(5).delete(6);
    assert_eq!(text_of(&doc.apply(&change)), "Hello");
}

#[test]
fn compose_applies_formatting() {
    let doc = Delta::document("Hello");
    let change = Delta::new().retain_with(5, bold());
    let result = doc.apply(&change);
    assert_eq!(result.ops.len(), 1);
    assert_eq!(result.ops[0].attributes, bold());
}

#[test]
fn compose_removes_formatting_with_null() {
    let doc = Delta::new().insert_with("Hello", bold());
    let change = Delta::new().retain_with(5, attrs(&[("bold", serde_json::Value::Null)]));
    let result = doc.apply(&change);
    assert!(
        result.ops[0].attributes.is_empty(),
        "null should strip the attribute"
    );
}

#[test]
fn compose_of_insert_then_delete_cancels_out() {
    let a = Delta::new().insert("abc");
    let b = Delta::new().delete(3);
    assert!(compose(&a, &b).is_empty());
}

#[test]
fn compose_is_associative() {
    let doc = Delta::document("The quick brown fox");
    let a = Delta::new().retain(4).insert("very ");
    let b = Delta::new().retain(14).delete(6);
    let c = Delta::new().retain_with(3, bold());

    let left = compose(&compose(&doc, &a), &compose(&b, &c));
    let right = compose(&compose(&compose(&doc, &a), &b), &c);
    assert_eq!(left, right);
}

// --- transform ----------------------------------------------------------

#[test]
fn transform_shifts_concurrent_insert() {
    let a = Delta::new().insert("aa");
    let b = Delta::new().retain(1).insert("b");
    // b happened against the same base, so it must shift right past a's insert.
    assert_eq!(transform(&a, &b, true), Delta::new().retain(3).insert("b"));
}

#[test]
fn transform_breaks_insert_ties_by_priority() {
    let a = Delta::new().insert("a");
    let b = Delta::new().insert("b");
    // With priority, a is treated as first, so b lands after it.
    assert_eq!(transform(&a, &b, true), Delta::new().retain(1).insert("b"));
    // Without priority, b wins the position.
    assert_eq!(transform(&a, &b, false), Delta::new().insert("b"));
}

#[test]
fn transform_drops_ops_targeting_already_deleted_text() {
    let a = Delta::new().delete(3);
    let b = Delta::new().retain(1).insert("x");
    // The text b retained is gone, so only the insert survives, at the front.
    assert_eq!(transform(&a, &b, true), Delta::new().insert("x"));
}

#[test]
fn transform_of_two_deletes_does_not_double_delete() {
    let a = Delta::new().retain(1).delete(2);
    let b = Delta::new().retain(1).delete(2);
    assert!(transform(&a, &b, true).is_empty(), "already deleted by a");
}

#[test]
fn transform_resolves_attribute_conflicts_by_priority() {
    let a = Delta::new().retain_with(3, attrs(&[("bold", json!(true))]));
    let b = Delta::new().retain_with(3, attrs(&[("bold", json!(false))]));
    assert!(
        transform(&a, &b, true).is_empty(),
        "a wins, b's conflicting format drops"
    );
    assert_eq!(transform(&a, &b, false), b, "b wins and passes through");
}

/// The transformation property: the whole point of the algorithm.
fn assert_converges(doc: &Delta, a: &Delta, b: &Delta) {
    let a_then_b = compose(&compose(doc, a), &transform(a, b, true));
    let b_then_a = compose(&compose(doc, b), &transform(b, a, false));
    assert_eq!(
        a_then_b, b_then_a,
        "divergence\n  doc: {doc:?}\n  a:   {a:?}\n  b:   {b:?}\n  a→b: {a_then_b:?}\n  b→a: {b_then_a:?}"
    );
}

#[test]
fn convergence_on_handpicked_cases() {
    let doc = Delta::document("Hello world");
    let cases: Vec<(Delta, Delta)> = vec![
        (Delta::new().insert("A"), Delta::new().insert("B")),
        (
            Delta::new().retain(5).insert(" there"),
            Delta::new().retain(11).insert("!"),
        ),
        (Delta::new().delete(5), Delta::new().retain(2).insert("XYZ")),
        (Delta::new().delete(6), Delta::new().delete(6)),
        (
            Delta::new().retain(5).delete(6),
            Delta::new().retain(3).delete(4),
        ),
        (
            Delta::new().retain_with(5, bold()),
            Delta::new().retain(2).insert("X"),
        ),
        (
            Delta::new().retain(5).delete(6),
            Delta::new().retain_with(11, bold()),
        ),
        (
            Delta::new().insert("start ").delete(5),
            Delta::new().retain(6).insert("mid"),
        ),
        (
            Delta::new().retain(2).delete(3).insert("zz"),
            Delta::new().retain(1).delete(9).insert("q"),
        ),
    ];
    for (a, b) in cases {
        assert_converges(&doc, &a, &b);
    }
}

#[test]
fn convergence_with_astral_characters() {
    // Emoji are two UTF-16 units; offsets must stay consistent throughout.
    let doc = Delta::document("a🌊b🌊c");
    assert_eq!(doc.len(), 7);
    let cases: Vec<(Delta, Delta)> = vec![
        (
            Delta::new().retain(1).insert("🎉"),
            Delta::new().retain(3).insert("X"),
        ),
        (
            Delta::new().retain(1).delete(2),
            Delta::new().retain(4).delete(2),
        ),
        (Delta::new().delete(3), Delta::new().retain(1).insert("🌟")),
        (
            Delta::new().retain_with(3, bold()),
            Delta::new().retain(1).delete(2),
        ),
    ];
    for (a, b) in cases {
        assert_converges(&doc, &a, &b);
    }
}

// --- transform_position -------------------------------------------------

#[test]
fn cursor_shifts_past_earlier_insert() {
    let change = Delta::new().insert("abc");
    assert_eq!(transform_position(&change, 5, false), 8);
}

#[test]
fn cursor_at_insertion_point_respects_priority() {
    let change = Delta::new().retain(3).insert("xyz");
    // Your own cursor rides along with your typing...
    assert_eq!(transform_position(&change, 3, false), 6);
    // ...someone else's stays put.
    assert_eq!(transform_position(&change, 3, true), 3);
}

#[test]
fn cursor_inside_deleted_range_collapses_to_its_start() {
    let change = Delta::new().retain(2).delete(5);
    assert_eq!(transform_position(&change, 4, false), 2);
    assert_eq!(transform_position(&change, 9, false), 4);
    assert_eq!(transform_position(&change, 1, false), 1);
}

// --- invert -------------------------------------------------------------

#[test]
fn invert_undoes_insert() {
    let doc = Delta::document("Hello");
    let change = Delta::new().retain(5).insert(" world");
    let undo = change.invert(&doc);
    assert_eq!(doc.apply(&change).apply(&undo), doc);
}

#[test]
fn invert_undoes_delete_restoring_exact_text() {
    let doc = Delta::document("Hello world");
    let change = Delta::new().retain(5).delete(6);
    let undo = change.invert(&doc);
    assert_eq!(doc.apply(&change).apply(&undo), doc);
}

#[test]
fn invert_undoes_formatting() {
    let doc = Delta::new().insert_with("Hello", bold());
    let change = Delta::new().retain_with(5, attrs(&[("bold", serde_json::Value::Null)]));
    let undo = change.invert(&doc);
    assert_eq!(doc.apply(&change).apply(&undo), doc);
}

#[test]
fn invert_undoes_mixed_change() {
    let doc = Delta::new().insert("Hello ").insert_with("world", bold());
    let change = Delta::new()
        .retain(2)
        .delete(4)
        .insert("XY")
        .retain_with(3, attrs(&[("italic", json!(true))]));
    let undo = change.invert(&doc);
    assert_eq!(doc.apply(&change).apply(&undo), doc);
}

// --- slice --------------------------------------------------------------

#[test]
fn slice_extracts_a_range_preserving_attributes() {
    let doc = Delta::new().insert("Hello ").insert_with("world", bold());
    assert_eq!(text_of(&doc.slice(3, 8)), "lo wo");
    let tail = doc.slice(6, 11);
    assert_eq!(text_of(&tail), "world");
    assert_eq!(tail.ops[0].attributes, bold());
}

// --- diff ---------------------------------------------------------------

#[test]
fn diff_detects_common_edit_shapes() {
    let cases = [
        ("hello", "hello world"), // append
        ("hello", "hi"),          // replace middle
        ("hello", ""),            // clear
        ("", "hello"),            // fill
        ("hello", "hello"),       // no change
        ("abc", "aXc"),           // single-char replace
        ("the fox", "the quick fox"),
        ("a🌊b", "a🌊🎉b"),
        ("a🌊b", "ab"),
    ];
    for (before, after) in cases {
        let d = diff_text(before, after);
        let result = Delta::document(before).apply(&d);
        assert_eq!(
            text_of(&result),
            after,
            "diff {before:?} -> {after:?} produced {d:?}"
        );
    }
}

#[test]
fn diff_of_identical_text_is_empty() {
    assert!(diff_text("same", "same").is_empty());
}

#[test]
fn diff_produces_minimal_affixes() {
    // Only the differing middle should be touched.
    let d = diff_text("the quick fox", "the slow fox");
    assert_eq!(d, Delta::new().retain(4).delete(5).insert("slow"));
}

// --- ServerDoc ----------------------------------------------------------

#[test]
fn server_applies_sequential_ops() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("Hello"), "alice")
        .unwrap();
    doc.apply(1, &Delta::new().retain(5).insert(" world"), "alice")
        .unwrap();
    assert_eq!(doc.to_plain_text(), "Hello world");
    assert_eq!(doc.revision(), 2);
}

#[test]
fn server_rebases_a_stale_concurrent_op() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("Hello"), "alice")
        .unwrap();

    // Both clients are at revision 1 and edit concurrently.
    doc.apply(1, &Delta::new().retain(5).insert(" world"), "alice")
        .unwrap();
    // Bob is still on revision 1 and prepends; his op must be rebased.
    let committed = doc.apply(1, &Delta::new().insert("Say: "), "bob").unwrap();

    assert_eq!(doc.to_plain_text(), "Say: Hello world");
    assert_eq!(committed.revision, 3);
    assert_eq!(committed.delta, Delta::new().insert("Say: "));
}

#[test]
fn server_rejects_revisions_from_the_future() {
    let mut doc = ServerDoc::new();
    let err = doc
        .apply(7, &Delta::new().insert("x"), "alice")
        .unwrap_err();
    assert_eq!(
        err,
        OtError::RevisionInFuture {
            requested: 7,
            current: 0
        }
    );
}

#[test]
fn server_rejects_ops_longer_than_the_document() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("hi"), "alice").unwrap();
    let err = doc
        .apply(1, &Delta::new().retain(500).insert("x"), "bob")
        .unwrap_err();
    assert!(matches!(err, OtError::LengthMismatch { .. }));
    assert_eq!(doc.to_plain_text(), "hi", "document must be left untouched");
}

#[test]
fn server_playback_reconstructs_earlier_revisions() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("Hello"), "alice")
        .unwrap();
    doc.apply(1, &Delta::new().retain(5).insert(" world"), "bob")
        .unwrap();
    doc.apply(2, &Delta::new().retain(11).insert("!"), "alice")
        .unwrap();
    doc.apply(3, &Delta::new().delete(6), "bob").unwrap();

    // The final delete(6) removes the leading "Hello ".
    assert_eq!(text_of(&doc.at_revision(4).unwrap()), "world!");
    assert_eq!(text_of(&doc.at_revision(3).unwrap()), "Hello world!");
    assert_eq!(text_of(&doc.at_revision(2).unwrap()), "Hello world");
    assert_eq!(text_of(&doc.at_revision(1).unwrap()), "Hello");
    assert_eq!(text_of(&doc.at_revision(0).unwrap()), "");
}

#[test]
fn server_playback_preserves_formatting_history() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("Hello"), "alice")
        .unwrap();
    doc.apply(1, &Delta::new().retain_with(5, bold()), "bob")
        .unwrap();

    let before = doc.at_revision(1).unwrap();
    assert!(
        before.ops[0].attributes.is_empty(),
        "formatting should be undone"
    );
    assert_eq!(doc.content().ops[0].attributes, bold());
}

#[test]
fn replay_from_op_log_matches_live_document() {
    let mut doc = ServerDoc::new();
    let mut log = Vec::new();
    for entry in [
        Delta::new().insert("Hello"),
        Delta::new().retain(5).insert(" world"),
        Delta::new().retain_with(5, bold()),
        Delta::new().retain(5).delete(6),
    ] {
        let rev = doc.apply(doc.revision(), &entry, "alice").unwrap();
        log.push(rev.delta);
    }
    assert_eq!(replay(&log, log.len()), *doc.content());
    assert_eq!(text_of(&replay(&log, 2)), "Hello world");
}

#[test]
fn concurrent_clients_converge_through_the_server() {
    // Three clients all start at revision 0 and submit without seeing each other.
    let mut server = ServerDoc::new();
    server
        .apply(0, &Delta::new().insert("The quick brown fox"), "seed")
        .unwrap();
    let base = server.revision();

    let ops = [
        (Delta::new().retain(4).insert("very "), "alice"),
        (Delta::new().retain(10).delete(6), "bob"),
        (Delta::new().retain_with(3, bold()), "carol"),
    ];

    // Each client mirrors the server by transforming what it receives.
    let mut mirrors = vec![server.content().clone(); 3];
    let mut broadcast = Vec::new();
    for (delta, author) in &ops {
        let rev = server.apply(base, delta, *author).unwrap();
        broadcast.push(rev.delta);
    }
    for mirror in &mut mirrors {
        for delta in &broadcast {
            *mirror = compose(mirror, delta);
        }
    }
    for mirror in &mirrors {
        assert_eq!(mirror, server.content(), "client diverged from server");
    }
}

#[test]
fn rollback_restores_the_exact_previous_state() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("Hello world"), "alice")
        .unwrap();
    doc.apply(1, &Delta::new().retain_with(5, bold()), "bob")
        .unwrap();
    let before = doc.content().clone();
    let revision = doc.revision();

    // A delete carries the removed text in its inverse, so this is the hardest case.
    doc.apply(2, &Delta::new().retain(5).delete(6), "carol")
        .unwrap();
    assert_eq!(doc.to_plain_text(), "Hello");

    assert!(doc.rollback_last());
    assert_eq!(doc.content(), &before, "content must be byte-identical");
    assert_eq!(doc.revision(), revision, "revision must go back");

    // And the document is still usable: the next op commits at the same revision
    // the rolled-back one would have taken, leaving no hole in the log.
    let next = doc
        .apply(revision, &Delta::new().retain(11).insert("!"), "dave")
        .unwrap();
    assert_eq!(next.revision, revision + 1);
    assert_eq!(doc.to_plain_text(), "Hello world!");
}

#[test]
fn rollback_on_an_empty_document_is_a_no_op() {
    let mut doc = ServerDoc::new();
    assert!(!doc.rollback_last());
    assert_eq!(doc.revision(), 0);
}

// --- hostile input -------------------------------------------------------

#[test]
fn validate_does_not_overflow_on_a_hostile_length() {
    // Reported reachable panic: `{"ops":[{"retain":1},{"delete":u64::MAX}]}`
    // reaches validate(), where `cursor + remaining` overflowed. Under
    // `overflow-checks` that aborted the whole process (panic = "abort").
    let doc = Delta::document("hi");
    let hostile = Delta::from_ops(vec![Op::retain(1), Op::delete(usize::MAX)]);
    assert!(matches!(
        validate(&hostile, &doc),
        Err(InvalidChange::PastEnd { .. })
    ));

    let hostile = Delta::from_ops(vec![Op::retain(usize::MAX)]);
    assert!(matches!(
        validate(&hostile, &doc),
        Err(InvalidChange::PastEnd { .. })
    ));
}

#[test]
fn lengths_saturate_instead_of_wrapping() {
    // A wrapped length would be small, and could then pass a bounds check that
    // the real length would fail.
    let d = Delta::from_ops(vec![Op::retain(usize::MAX), Op::retain(10)]);
    assert_eq!(
        d.len(),
        usize::MAX,
        "must saturate, not wrap to a small value"
    );

    let d = Delta::from_ops(vec![Op::delete(usize::MAX), Op::delete(10)]);
    assert_eq!(d.base_length(), usize::MAX);
}

#[test]
fn a_hostile_op_is_refused_without_touching_the_document() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("hi"), "alice").unwrap();
    let before = doc.to_plain_text();

    let hostile = Delta::from_ops(vec![Op::retain(1), Op::delete(usize::MAX)]);
    assert!(doc.apply(1, &hostile, "mallory").is_err());
    assert_eq!(doc.to_plain_text(), before, "document must be untouched");
    assert_eq!(doc.revision(), 1, "revision must not advance");
}

/// The example printed in README.md and on the crates.io page.
///
/// Kept as a test so a published front-page example cannot silently rot.
#[test]
fn readme_example_is_correct() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("Hello world"), "alice")
        .unwrap();

    // Two clients edit concurrently, both against revision 1.
    doc.apply(1, &Delta::new().retain(5).insert(" there"), "bob")
        .unwrap();
    doc.apply(1, &Delta::new().insert("Say: "), "carol")
        .unwrap();

    assert_eq!(doc.to_plain_text(), "Say: Hello there world");
}

// --- randomised convergence ---------------------------------------------

/// Small deterministic PRNG (xorshift64*), so the property test is reproducible
/// and the crate stays dependency-free.
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

/// UTF-16 offsets at which a character starts in `doc`, plus its total length.
///
/// Random ops are snapped to these so they never split a surrogate pair, which
/// is the same guarantee a real browser client provides.
fn boundaries(doc: &Delta) -> Vec<usize> {
    let mut offsets = vec![0usize];
    let mut offset = 0usize;
    for ch in doc.to_plain_text().chars() {
        offset += ch.len_utf16();
        offsets.push(offset);
    }
    offsets
}

/// Build a random but structurally valid change against a document whose
/// character boundaries are `bounds`.
fn random_delta(rng: &mut Rng, bounds: &[usize]) -> Delta {
    let last = bounds.len() - 1;
    let mut delta = Delta::new();
    let mut at = 0usize; // index into `bounds`

    while at < last {
        // Choose an end boundary strictly after the current one.
        let span = |rng: &mut Rng, at: usize| -> (usize, usize) {
            let end = at + 1 + rng.below(last - at);
            (bounds[end] - bounds[at], end)
        };
        match rng.below(4) {
            0 => {
                let (n, end) = span(rng, at);
                delta.push(Op::retain(n));
                at = end;
            }
            1 => {
                let (n, end) = span(rng, at);
                delta.push(Op::delete(n));
                at = end;
            }
            2 => {
                let words = ["cat", "🌊", "xyz", "the ", "é"];
                delta.push(Op::insert(words[rng.below(words.len())]));
            }
            _ => {
                let (n, end) = span(rng, at);
                let keys = ["bold", "italic", "link"];
                let key = keys[rng.below(keys.len())];
                let value = if rng.below(3) == 0 {
                    serde_json::Value::Null
                } else {
                    json!(true)
                };
                delta.push(Op::retain(n).with_attr(key, value));
                at = end;
            }
        }
        // Occasionally stop early, leaving a trailing implicit retain.
        if rng.below(8) == 0 {
            break;
        }
    }
    delta.chop()
}

// --- validation ---------------------------------------------------------

#[test]
fn validate_accepts_well_formed_changes() {
    let doc = Delta::document("a🌊b");
    assert!(validate(&Delta::new().retain(1).delete(2), &doc).is_ok());
    assert!(validate(&Delta::new().retain(4), &doc).is_ok());
    assert!(validate(&Delta::new().insert("hi"), &doc).is_ok());
}

#[test]
fn validate_rejects_boundary_inside_a_surrogate_pair() {
    let doc = Delta::document("a🌊b");
    // Offset 2 is between the high and low surrogate of the emoji.
    assert_eq!(
        validate(&Delta::new().retain(1).delete(1), &doc),
        Err(InvalidChange::SplitCharacter { offset: 2 })
    );
    assert_eq!(
        validate(&Delta::new().retain(2), &doc),
        Err(InvalidChange::SplitCharacter { offset: 2 })
    );
}

#[test]
fn validate_rejects_changes_past_the_end() {
    let doc = Delta::document("hi");
    assert_eq!(
        validate(&Delta::new().retain(9), &doc),
        Err(InvalidChange::PastEnd {
            needed: 9,
            doc_len: 2
        })
    );
}

#[test]
fn validate_counts_an_embed_as_one_unit() {
    let doc = Delta::new()
        .insert("a")
        .embed(json!({"image": "x.png"}))
        .insert("b");
    assert!(validate(&Delta::new().retain(1).delete(1), &doc).is_ok());
    assert!(validate(&Delta::new().retain(3), &doc).is_ok());
}

#[test]
fn server_rejects_ops_that_split_a_character() {
    let mut doc = ServerDoc::new();
    doc.apply(0, &Delta::new().insert("a🌊b"), "alice").unwrap();
    let err = doc
        .apply(1, &Delta::new().retain(1).delete(1), "mallory")
        .unwrap_err();
    assert_eq!(err, OtError::SplitCharacter { offset: 2 });
    assert_eq!(
        doc.to_plain_text(),
        "a🌊b",
        "document must be left untouched"
    );
}

#[test]
fn convergence_property_over_random_operations() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let docs = [
        Delta::document("Hello world"),
        Delta::document("The quick brown fox jumps over the lazy dog"),
        Delta::document("a🌊b🌊c émoji"),
        Delta::new()
            .insert("mixed ")
            .insert_with("formatting", bold()),
    ];

    for round in 0..4000 {
        let doc = &docs[rng.below(docs.len())];
        let bounds = boundaries(doc);
        let a = random_delta(&mut rng, &bounds);
        let b = random_delta(&mut rng, &bounds);

        let a_then_b = compose(&compose(doc, &a), &transform(&a, &b, true));
        let b_then_a = compose(&compose(doc, &b), &transform(&b, &a, false));

        assert_eq!(
            a_then_b, b_then_a,
            "round {round} diverged\n  doc: {doc:?}\n  a: {a:?}\n  b: {b:?}"
        );
    }
}

#[test]
fn invert_property_over_random_operations() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_1234);
    let doc = Delta::new()
        .insert("The quick ")
        .insert_with("brown fox", bold())
        .insert(" jumps");

    let bounds = boundaries(&doc);
    for round in 0..2000 {
        let change = random_delta(&mut rng, &bounds);
        let applied = compose(&doc, &change);
        let undo = invert(&change, &doc);
        assert_eq!(
            compose(&applied, &undo),
            doc,
            "round {round}: undo failed for {change:?}"
        );
    }
}

#[test]
fn server_converges_under_random_concurrent_load() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut server = ServerDoc::new();
    let seed = server
        .apply(0, &Delta::new().insert("Collaborative editing"), "seed")
        .unwrap();

    // Simulate clients submitting against stale revisions, as happens in flight.
    // The log must start from the very first op so a replay begins from empty.
    let mut committed: Vec<Delta> = vec![seed.delta];
    for _ in 0..500 {
        let current = server.revision();
        let lag = rng.below(4).min(current as usize);
        let client_rev = current - lag as u64;

        // The client writes against the document as it saw it at client_rev.
        let Some(client_view) = server.at_revision(client_rev) else {
            continue;
        };
        let change = random_delta(&mut rng, &boundaries(&client_view));
        if change.is_empty() {
            continue;
        }
        if let Ok(rev) = server.apply(client_rev, &change, "client") {
            committed.push(rev.delta);
        }
    }

    // A fresh client replaying the broadcast log must land on the same document.
    let mut mirror = Delta::new();
    for delta in &committed {
        mirror = compose(&mirror, delta);
    }
    assert_eq!(&mirror, server.content(), "replay diverged from the server");
    assert!(
        server.content().is_document(),
        "document must contain only inserts"
    );
}
