//! Deltas: the document and change representation.
//!
//! A [`Delta`] is a flat sequence of ops. Used as a *document* it contains only
//! inserts; used as a *change* it may also contain retains and deletes that
//! address an implied base document. The two are the same type on purpose —
//! applying a change is just `compose(document, change)`.
//!
//! The JSON encoding is deliberately identical to the widely-used `quill-delta`
//! format (`{"ops":[{"insert":"hi"},{"retain":3,"attributes":{"bold":true}}]}`),
//! so the browser client and the server speak the same wire language.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attributes::{self, Attributes};
use crate::text::{advance_utf16, utf16_len, utf16_slice};

/// Sentinel used for "the rest of this op" and for the length of an exhausted
/// iterator, mirroring `Infinity` in the reference implementation.
const INFINITY: usize = usize::MAX;

/// The payload of an insert op: literal text, or an opaque embedded object
/// (image, attachment, gadget) which always occupies exactly one unit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Insert {
    Text(String),
    Embed(Value),
}

impl Insert {
    /// Length in UTF-16 code units; embeds are atomic and count as one.
    pub fn len(&self) -> usize {
        match self {
            Insert::Text(s) => utf16_len(s),
            Insert::Embed(_) => 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What an op does to the document.
#[derive(Clone, Debug, PartialEq)]
pub enum OpKind {
    /// Add new content.
    Insert(Insert),
    /// Advance over existing content, optionally re-formatting it.
    Retain(usize),
    /// Remove existing content.
    Delete(usize),
}

/// Discriminant of [`OpKind`], for cheap comparisons during transformation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Insert,
    Retain,
    Delete,
}

/// A single op: an action plus the formatting it carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "OpRepr", into = "OpRepr")]
pub struct Op {
    pub kind: OpKind,
    pub attributes: Attributes,
}

/// Wire representation. Exactly one of the three action fields is present.
#[derive(Serialize, Deserialize)]
struct OpRepr {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    insert: Option<Insert>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    retain: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    delete: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    attributes: Option<Attributes>,
}

impl TryFrom<OpRepr> for Op {
    type Error = String;

    fn try_from(repr: OpRepr) -> Result<Self, Self::Error> {
        let kind = match (repr.insert, repr.retain, repr.delete) {
            (Some(i), None, None) => OpKind::Insert(i),
            (None, Some(r), None) => OpKind::Retain(r),
            (None, None, Some(d)) => OpKind::Delete(d),
            (None, None, None) => return Err("op has no insert, retain or delete".into()),
            _ => return Err("op must have exactly one of insert, retain or delete".into()),
        };
        Ok(Op {
            kind,
            attributes: repr.attributes.unwrap_or_default(),
        })
    }
}

impl From<Op> for OpRepr {
    fn from(op: Op) -> Self {
        let attributes = if op.attributes.is_empty() {
            None
        } else {
            Some(op.attributes)
        };
        match op.kind {
            OpKind::Insert(i) => OpRepr {
                insert: Some(i),
                retain: None,
                delete: None,
                attributes,
            },
            OpKind::Retain(r) => OpRepr {
                insert: None,
                retain: Some(r),
                delete: None,
                attributes,
            },
            OpKind::Delete(d) => OpRepr {
                insert: None,
                retain: None,
                delete: Some(d),
                attributes,
            },
        }
    }
}

impl Op {
    pub fn insert(text: impl Into<String>) -> Self {
        Op {
            kind: OpKind::Insert(Insert::Text(text.into())),
            attributes: Attributes::new(),
        }
    }

    pub fn embed(value: Value) -> Self {
        Op {
            kind: OpKind::Insert(Insert::Embed(value)),
            attributes: Attributes::new(),
        }
    }

    pub fn retain(n: usize) -> Self {
        Op {
            kind: OpKind::Retain(n),
            attributes: Attributes::new(),
        }
    }

    pub fn delete(n: usize) -> Self {
        Op {
            kind: OpKind::Delete(n),
            attributes: Attributes::new(),
        }
    }

    pub fn with_attributes(mut self, attributes: Attributes) -> Self {
        self.attributes = attributes;
        self
    }

    pub fn with_attr(mut self, key: &str, value: Value) -> Self {
        self.attributes.insert(key.to_string(), value);
        self
    }

    /// How much of the document this op spans.
    pub fn len(&self) -> usize {
        match &self.kind {
            OpKind::Insert(i) => i.len(),
            OpKind::Retain(n) | OpKind::Delete(n) => *n,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn kind(&self) -> Kind {
        match self.kind {
            OpKind::Insert(_) => Kind::Insert,
            OpKind::Retain(_) => Kind::Retain,
            OpKind::Delete(_) => Kind::Delete,
        }
    }

    fn text(&self) -> Option<&str> {
        match &self.kind {
            OpKind::Insert(Insert::Text(s)) => Some(s),
            _ => None,
        }
    }
}

/// A sequence of ops describing a document or a change to one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub ops: Vec<Op>,
}

impl Delta {
    pub fn new() -> Self {
        Delta::default()
    }

    /// A document consisting of a single unformatted run of text.
    pub fn document(text: impl Into<String>) -> Self {
        let mut d = Delta::new();
        d.push(Op::insert(text));
        d
    }

    pub fn from_ops(ops: Vec<Op>) -> Self {
        let mut d = Delta::new();
        for op in ops {
            d.push(op);
        }
        d
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    // --- builders -------------------------------------------------------

    pub fn insert(mut self, text: impl Into<String>) -> Self {
        self.push(Op::insert(text));
        self
    }

    pub fn insert_with(mut self, text: impl Into<String>, attributes: Attributes) -> Self {
        self.push(Op::insert(text).with_attributes(attributes));
        self
    }

    pub fn embed(mut self, value: Value) -> Self {
        self.push(Op::embed(value));
        self
    }

    pub fn retain(mut self, n: usize) -> Self {
        self.push(Op::retain(n));
        self
    }

    pub fn retain_with(mut self, n: usize, attributes: Attributes) -> Self {
        self.push(Op::retain(n).with_attributes(attributes));
        self
    }

    pub fn delete(mut self, n: usize) -> Self {
        self.push(Op::delete(n));
        self
    }

    /// Append an op, merging it into the previous one where possible so the
    /// representation stays canonical.
    pub fn push(&mut self, new_op: Op) {
        if new_op.is_empty() {
            return;
        }
        let mut index = self.ops.len();
        if index == 0 {
            self.ops.push(new_op);
            return;
        }

        // Merge adjacent deletes. Lengths arrive from the wire, so these sums
        // saturate rather than wrap: a hostile pair of ops must not be able to
        // produce a nonsensical length, nor panic under `overflow-checks`.
        if let (OpKind::Delete(a), OpKind::Delete(b)) = (&self.ops[index - 1].kind, &new_op.kind) {
            self.ops[index - 1].kind = OpKind::Delete(a.saturating_add(*b));
            return;
        }

        // Inserting at a position that is also being deleted is order-agnostic;
        // canonicalise by always placing the insert first. This keeps deltas
        // that differ only in op order structurally equal.
        if self.ops[index - 1].kind() == Kind::Delete && new_op.kind() == Kind::Insert {
            index -= 1;
            if index == 0 {
                self.ops.insert(0, new_op);
                return;
            }
        }

        if self.ops[index - 1].attributes == new_op.attributes {
            let prev = &self.ops[index - 1];
            match (prev.text(), new_op.text()) {
                (Some(a), Some(b)) => {
                    let merged = format!("{a}{b}");
                    self.ops[index - 1].kind = OpKind::Insert(Insert::Text(merged));
                    return;
                }
                _ => {
                    if let (OpKind::Retain(a), OpKind::Retain(b)) = (&prev.kind, &new_op.kind) {
                        self.ops[index - 1].kind = OpKind::Retain(a.saturating_add(*b));
                        return;
                    }
                }
            }
        }

        self.ops.insert(index, new_op);
    }

    /// Drop a trailing bare retain — it is a no-op that only adds noise.
    pub fn chop(mut self) -> Self {
        if let Some(last) = self.ops.last() {
            if last.kind() == Kind::Retain && last.attributes.is_empty() {
                self.ops.pop();
            }
        }
        self
    }

    // --- measurements ---------------------------------------------------

    /// Total span of every op, i.e. base length plus inserted length.
    ///
    /// Saturating: op lengths are client-supplied, and a delta crafted to sum
    /// past `usize::MAX` must not wrap into a small value that later passes a
    /// bounds check.
    pub fn len(&self) -> usize {
        self.ops
            .iter()
            .map(Op::len)
            .fold(0usize, usize::saturating_add)
    }

    /// Length of the document this delta addresses as a change.
    pub fn base_length(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op.kind, OpKind::Retain(_) | OpKind::Delete(_)))
            .map(Op::len)
            .fold(0usize, usize::saturating_add)
    }

    /// Length of the document produced by applying this delta.
    pub fn target_length(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op.kind, OpKind::Retain(_) | OpKind::Insert(_)))
            .map(Op::len)
            .fold(0usize, usize::saturating_add)
    }

    /// Number of units this delta adds to (or removes from) a document.
    pub fn change_length(&self) -> isize {
        self.target_length() as isize - self.base_length() as isize
    }

    /// Plain text of a document delta, with embeds collapsed to `\u{fffc}`
    /// (OBJECT REPLACEMENT CHARACTER) so offsets stay faithful.
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        for op in &self.ops {
            match &op.kind {
                OpKind::Insert(Insert::Text(s)) => out.push_str(s),
                OpKind::Insert(Insert::Embed(_)) => out.push('\u{fffc}'),
                _ => {}
            }
        }
        out
    }

    /// True if this delta contains only inserts, i.e. it is a valid document.
    pub fn is_document(&self) -> bool {
        self.ops.iter().all(|op| op.kind() == Kind::Insert)
    }

    // --- core algebra ---------------------------------------------------

    /// Apply `change` to this document, returning the new document.
    pub fn apply(&self, change: &Delta) -> Delta {
        compose(self, change)
    }

    /// Sequential composition: `compose(a, b)` has the same effect as applying
    /// `a` then `b`.
    pub fn compose(&self, other: &Delta) -> Delta {
        compose(self, other)
    }

    /// Concurrent transformation. See [`transform`].
    pub fn transform(&self, other: &Delta, priority: bool) -> Delta {
        transform(self, other, priority)
    }

    /// Map a cursor position across this delta. See [`transform_position`].
    pub fn transform_position(&self, index: usize, priority: bool) -> usize {
        transform_position(self, index, priority)
    }

    /// The change that undoes this one when applied to `base`.
    pub fn invert(&self, base: &Delta) -> Delta {
        invert(self, base)
    }

    /// Extract `[start, end)` of a document delta as its own document.
    pub fn slice(&self, start: usize, end: usize) -> Delta {
        let mut out = Delta::new();
        let mut iter = OpIterator::new(&self.ops);
        let mut index = 0usize;
        while index < end && iter.has_next() {
            let next_op = if index < start {
                iter.next(start - index)
            } else {
                let op = iter.next(end - index);
                out.push(op.clone());
                op
            };
            index += next_op.len();
        }
        out
    }
}

/// Walks a slice of ops, able to split an op at an arbitrary offset.
///
/// The byte offset is tracked alongside the UTF-16 offset so that repeatedly
/// slicing a long text insert stays linear rather than quadratic.
pub(crate) struct OpIterator<'a> {
    ops: &'a [Op],
    index: usize,
    offset: usize,
    byte_offset: usize,
}

impl<'a> OpIterator<'a> {
    pub(crate) fn new(ops: &'a [Op]) -> Self {
        OpIterator {
            ops,
            index: 0,
            offset: 0,
            byte_offset: 0,
        }
    }

    pub(crate) fn has_next(&self) -> bool {
        self.peek_length() < INFINITY
    }

    /// Remaining length of the current op, or `INFINITY` when exhausted.
    pub(crate) fn peek_length(&self) -> usize {
        match self.ops.get(self.index) {
            Some(op) => op.len() - self.offset,
            None => INFINITY,
        }
    }

    /// Kind of the current op. An exhausted iterator reports `Retain`, which
    /// makes it behave like an infinite run of untouched document.
    pub(crate) fn peek_kind(&self) -> Kind {
        match self.ops.get(self.index) {
            Some(op) => op.kind(),
            None => Kind::Retain,
        }
    }

    /// Take up to `length` units from the current op.
    pub(crate) fn next(&mut self, length: usize) -> Op {
        let Some(op) = self.ops.get(self.index) else {
            return Op::retain(INFINITY);
        };
        let op_len = op.len();
        let take = length.min(op_len - self.offset);

        let result = match &op.kind {
            OpKind::Delete(_) => Op::delete(take),
            OpKind::Retain(_) => Op::retain(take).with_attributes(op.attributes.clone()),
            OpKind::Insert(Insert::Embed(v)) => {
                Op::embed(v.clone()).with_attributes(op.attributes.clone())
            }
            OpKind::Insert(Insert::Text(s)) => {
                let start = self.byte_offset;
                let end = advance_utf16(s, start, take);
                self.byte_offset = end;
                Op::insert(&s[start..end]).with_attributes(op.attributes.clone())
            }
        };

        self.offset += take;
        if self.offset >= op_len {
            self.index += 1;
            self.offset = 0;
            self.byte_offset = 0;
        }
        result
    }
}

/// Sequential composition.
///
/// `compose(a, b)` yields a single delta with the same effect as applying `a`
/// and then `b`. This is also how a change is applied to a document, since a
/// document is just a delta of inserts.
pub fn compose(a: &Delta, b: &Delta) -> Delta {
    let mut ia = OpIterator::new(&a.ops);
    let mut ib = OpIterator::new(&b.ops);
    let mut out = Delta::new();

    while ia.has_next() || ib.has_next() {
        if ib.peek_kind() == Kind::Insert {
            // b inserts new content; nothing in a corresponds to it.
            out.push(ib.next(INFINITY));
        } else if ia.peek_kind() == Kind::Delete {
            // a already removed this content; b never saw it.
            out.push(ia.next(INFINITY));
        } else {
            let length = ia.peek_length().min(ib.peek_length());
            let op_a = ia.next(length);
            let op_b = ib.next(length);

            match op_b.kind {
                OpKind::Retain(_) => {
                    // b formats (or passes over) whatever a produced here.
                    let keep_null = op_a.kind() == Kind::Retain;
                    let merged = attributes::compose(&op_a.attributes, &op_b.attributes, keep_null);
                    // Take the span from `length` rather than from `op_a`: an
                    // exhausted iterator reports an infinite retain, and cloning
                    // that kind would leak the sentinel into a real delta.
                    let kind = match op_a.kind {
                        OpKind::Insert(insert) => OpKind::Insert(insert),
                        _ => OpKind::Retain(length),
                    };
                    out.push(Op {
                        kind,
                        attributes: merged,
                    });
                }
                OpKind::Delete(_) if op_a.kind() == Kind::Retain => {
                    // b deletes content a merely passed over.
                    out.push(op_b);
                }
                // Remaining case: b deletes what a inserted, so both vanish.
                _ => {}
            }
        }
    }

    out.chop()
}

/// Concurrent transformation.
///
/// Given two deltas `a` and `b` derived from the same document, returns `b'`
/// such that `compose(a, b')` and `compose(b, transform(b, a, !priority))`
/// converge on the same document.
///
/// `priority` says that `a` is treated as having happened first, which only
/// matters for breaking ties between two inserts at the same position.
pub fn transform(a: &Delta, b: &Delta, priority: bool) -> Delta {
    let mut ia = OpIterator::new(&a.ops);
    let mut ib = OpIterator::new(&b.ops);
    let mut out = Delta::new();

    while ia.has_next() || ib.has_next() {
        if ia.peek_kind() == Kind::Insert && (priority || ib.peek_kind() != Kind::Insert) {
            // a's insert shifts b's ops to the right.
            out.push(Op::retain(ia.next(INFINITY).len()));
        } else if ib.peek_kind() == Kind::Insert {
            // b's insert survives untouched.
            out.push(ib.next(INFINITY));
        } else {
            let length = ia.peek_length().min(ib.peek_length());
            let op_a = ia.next(length);
            let op_b = ib.next(length);

            if op_a.kind() == Kind::Delete {
                // a already removed this content, so b's op has nothing to act on.
                continue;
            } else if op_b.kind() == Kind::Delete {
                out.push(op_b);
            } else {
                out.push(Op::retain(length).with_attributes(attributes::transform(
                    &op_a.attributes,
                    &op_b.attributes,
                    priority,
                )));
            }
        }
    }

    out.chop()
}

/// Map a cursor offset across `delta`.
///
/// `priority` shifts a cursor sitting exactly at an insertion point to the left
/// of the inserted text, which is what you want for *other* people's cursors so
/// they do not get dragged along by your typing.
pub fn transform_position(delta: &Delta, index: usize, priority: bool) -> usize {
    let mut index = index;
    let mut offset = 0usize;
    let mut iter = OpIterator::new(&delta.ops);

    while iter.has_next() && offset <= index {
        let length = iter.peek_length();
        let kind = iter.peek_kind();
        iter.next(INFINITY);

        match kind {
            Kind::Delete => {
                index -= length.min(index - offset);
                continue;
            }
            Kind::Insert if offset < index || !priority => {
                index += length;
            }
            _ => {}
        }
        offset += length;
    }
    index
}

/// Build the delta that undoes `change` when applied to `base`.
pub fn invert(change: &Delta, base: &Delta) -> Delta {
    let mut inverted = Delta::new();
    let mut base_index = 0usize;

    for op in &change.ops {
        match &op.kind {
            OpKind::Insert(_) => {
                inverted.push(Op::delete(op.len()));
            }
            OpKind::Retain(n) if op.attributes.is_empty() => {
                inverted.push(Op::retain(*n));
                base_index += n;
            }
            OpKind::Delete(n) | OpKind::Retain(n) => {
                let length = *n;
                let slice = base.slice(base_index, base_index + length);
                for base_op in &slice.ops {
                    if op.kind() == Kind::Delete {
                        // Re-insert exactly what was removed.
                        inverted.push(base_op.clone());
                    } else {
                        inverted.push(Op::retain(base_op.len()).with_attributes(
                            attributes::invert(&op.attributes, &base_op.attributes),
                        ));
                    }
                }
                base_index += length;
            }
        }
    }

    inverted.chop()
}

/// Why a change cannot be applied to a given document.
#[derive(Debug, PartialEq, Eq)]
pub enum InvalidChange {
    /// The change addresses more content than the document holds.
    PastEnd { needed: usize, doc_len: usize },
    /// A retain or delete boundary falls inside a multi-unit character.
    ///
    /// Real clients never produce this: the browser treats a surrogate pair as
    /// atomic for cursor movement. It is reachable only from a buggy or hostile
    /// peer, and applying it would corrupt the document, so it is refused.
    SplitCharacter { offset: usize },
}

/// Check that `change` can be applied to document `doc` without corrupting it.
///
/// Runs in a single linear pass over the document text. Embeds collapse to one
/// character in the plain-text projection, so offsets line up exactly.
pub fn validate(change: &Delta, doc: &Delta) -> Result<(), InvalidChange> {
    let text = doc.to_plain_text();
    let doc_len = utf16_len(&text);
    let mut cursor = 0usize;
    let mut byte = 0usize;

    for op in &change.ops {
        let span = match op.kind {
            OpKind::Insert(_) => continue,
            OpKind::Retain(n) | OpKind::Delete(n) => n,
        };
        let mut remaining = span;
        while remaining > 0 {
            let Some(ch) = text[byte..].chars().next() else {
                return Err(InvalidChange::PastEnd {
                    needed: cursor.saturating_add(remaining),
                    doc_len,
                });
            };
            let width = ch.len_utf16();
            if width > remaining {
                return Err(InvalidChange::SplitCharacter {
                    offset: cursor.saturating_add(remaining),
                });
            }
            remaining -= width;
            byte += ch.len_utf8();
            cursor += width;
        }
    }
    Ok(())
}

/// Compute a delta turning document `before` into document `after`.
///
/// Uses a common prefix/suffix scan rather than a full edit-distance diff. That
/// is exactly right for how text is actually edited (typing, pasting, deleting a
/// selection) and stays linear; a minimal diff would not change correctness,
/// only op count.
pub fn diff_text(before: &str, after: &str) -> Delta {
    if before == after {
        return Delta::new();
    }

    let before_units = utf16_len(before);
    let after_units = utf16_len(after);

    // Longest common prefix, measured in UTF-16 units but aligned to chars.
    let mut prefix = 0usize;
    {
        let mut a = before.chars();
        let mut b = after.chars();
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => prefix += x.len_utf16(),
                _ => break,
            }
        }
    }

    // Longest common suffix, not allowed to overlap the prefix.
    let max_suffix = before_units.min(after_units) - prefix;
    let mut suffix = 0usize;
    {
        let mut a = before.chars().rev();
        let mut b = after.chars().rev();
        loop {
            if suffix >= max_suffix {
                break;
            }
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => {
                    if suffix + x.len_utf16() > max_suffix {
                        break;
                    }
                    suffix += x.len_utf16();
                }
                _ => break,
            }
        }
    }

    let removed = before_units - prefix - suffix;
    let inserted = utf16_slice(after, prefix, after_units - suffix);

    let mut delta = Delta::new();
    delta.push(Op::retain(prefix));
    delta.push(Op::delete(removed));
    delta.push(Op::insert(inserted));
    delta.chop()
}
