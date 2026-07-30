//! Operational transformation for collaborative rich-text editing.
//!
//! This crate implements the concurrency-control core that made Apache Wave
//! work: many people typing into the same document at the same time, with every
//! participant converging on identical content without locking.
//!
//! # Model
//!
//! A [`Delta`] is a sequence of `insert` / `retain` / `delete` ops carrying
//! optional formatting [`Attributes`]. A document is simply a delta containing
//! only inserts, so applying a change is `compose(document, change)`.
//!
//! The two operations that make concurrency work:
//!
//! - [`compose(a, b)`](compose) — sequential: do `a`, then `b`, as one delta.
//! - [`transform(a, b, priority)`](transform) — concurrent: rewrite `b`, which
//!   was written against the same base as `a`, so it can be applied *after* `a`.
//!
//! Together they satisfy the transformation property, which is what guarantees
//! convergence:
//!
//! ```text
//! compose(a, transform(a, b, true)) == compose(b, transform(b, a, false))
//! ```
//!
//! # Server model
//!
//! [`ServerDoc`] owns the authoritative document plus a log of applied ops. A
//! client submits an op tagged with the revision it was written against; the
//! server transforms it forward over everything committed since, applies it, and
//! broadcasts the transformed op. This is the design Wave's server used, and it
//! is what lets clients edit optimistically without waiting for a round trip.
//!
//! # Offsets
//!
//! All lengths and positions are **UTF-16 code units**, matching the browser's
//! DOM selection APIs so the Rust and JavaScript engines agree exactly.

mod attributes;
mod delta;
mod text;

pub use attributes::{
    compose as compose_attributes, invert as invert_attributes, transform as transform_attributes,
    without_nulls, Attributes,
};
pub use delta::{
    compose, diff_text, invert, transform, transform_position, validate, Delta, Insert,
    InvalidChange, Kind, Op, OpKind,
};
pub use text::{utf16_len, utf16_slice};

use std::collections::VecDeque;

/// How many historical ops a document keeps for transforming late arrivals.
///
/// A client that falls further behind than this must resynchronise from a
/// snapshot. In practice a client is at most a few ops behind; this bound exists
/// so a long-lived document cannot grow its in-memory history without limit.
pub const MAX_HISTORY: usize = 2048;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum OtError {
    #[error("revision {requested} is newer than the server's revision {current}")]
    RevisionInFuture { requested: u64, current: u64 },

    #[error("revision {requested} is too old to transform against (oldest retained is {oldest}); resynchronise")]
    RevisionTooOld { requested: u64, oldest: u64 },

    #[error("operation spans {op_base} units but the document is {doc_len} units long")]
    LengthMismatch { op_base: usize, doc_len: usize },

    #[error("operation boundary at offset {offset} falls inside a character")]
    SplitCharacter { offset: usize },

    #[error("a document delta must contain only insert operations")]
    NotADocument,
}

/// An op as committed to a document's history.
#[derive(Clone, Debug)]
pub struct Revision {
    /// The revision number *produced* by applying this op.
    pub revision: u64,
    /// The op in its transformed, as-applied form. This is what peers receive.
    pub delta: Delta,
    /// The inverse of `delta`, captured against the document state immediately
    /// before it was applied. Stored at commit time because that is the only
    /// moment the pre-state is available, and it makes backward playback exact.
    pub undo: Delta,
    /// Opaque author identifier, carried through for attribution and playback.
    pub author: String,
}

/// The authoritative copy of a document plus enough history to transform
/// concurrent client submissions.
#[derive(Clone, Debug)]
pub struct ServerDoc {
    content: Delta,
    revision: u64,
    history: VecDeque<Revision>,
}

impl Default for ServerDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerDoc {
    /// An empty document at revision 0.
    pub fn new() -> Self {
        ServerDoc {
            content: Delta::new(),
            revision: 0,
            history: VecDeque::new(),
        }
    }

    /// Restore a document that was persisted at a known revision.
    pub fn from_snapshot(content: Delta, revision: u64) -> Result<Self, OtError> {
        if !content.is_document() {
            return Err(OtError::NotADocument);
        }
        Ok(ServerDoc {
            content,
            revision,
            history: VecDeque::new(),
        })
    }

    pub fn content(&self) -> &Delta {
        &self.content
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub fn to_plain_text(&self) -> String {
        self.content.to_plain_text()
    }

    /// The oldest revision this document can still transform against.
    pub fn oldest_retained(&self) -> u64 {
        self.history
            .front()
            .map(|r| r.revision - 1)
            .unwrap_or(self.revision)
    }

    /// Ops committed after `since`, in order.
    pub fn history_since(&self, since: u64) -> impl Iterator<Item = &Revision> {
        self.history.iter().filter(move |r| r.revision > since)
    }

    /// Apply a client op written against `client_revision`.
    ///
    /// The op is transformed forward over every op committed since that
    /// revision, then applied. Returns the transformed op — which is what must
    /// be broadcast to other clients — and the new revision number.
    ///
    /// Ops already in history take priority, so two clients inserting at the
    /// same offset produce a deterministic order rather than interleaving.
    pub fn apply(
        &mut self,
        client_revision: u64,
        delta: &Delta,
        author: impl Into<String>,
    ) -> Result<Revision, OtError> {
        if client_revision > self.revision {
            return Err(OtError::RevisionInFuture {
                requested: client_revision,
                current: self.revision,
            });
        }
        let oldest = self.oldest_retained();
        if client_revision < oldest {
            return Err(OtError::RevisionTooOld {
                requested: client_revision,
                oldest,
            });
        }

        // Rebase the incoming op onto the current document state.
        let mut rebased = delta.clone();
        for committed in self.history.iter().filter(|r| r.revision > client_revision) {
            rebased = transform(&committed.delta, &rebased, true);
        }

        // Refuse anything that would corrupt the document: an op reaching past
        // the end, or one whose boundaries split a multi-unit character. The
        // document is only ever mutated by validated ops, so it stays well-formed.
        delta::validate(&rebased, &self.content).map_err(|e| match e {
            delta::InvalidChange::PastEnd { needed, doc_len } => OtError::LengthMismatch {
                op_base: needed,
                doc_len,
            },
            delta::InvalidChange::SplitCharacter { offset } => OtError::SplitCharacter { offset },
        })?;

        let undo = invert(&rebased, &self.content);
        self.content = compose(&self.content, &rebased);
        self.revision += 1;

        let entry = Revision {
            revision: self.revision,
            delta: rebased,
            undo,
            author: author.into(),
        };
        self.history.push_back(entry.clone());
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
        Ok(entry)
    }

    /// Undo the most recently applied op, restoring the exact previous state.
    ///
    /// Exists so a caller that fails to *persist* an op can put the document
    /// back. Without it the in-memory document advances past storage, later ops
    /// transform over an op nobody else has, and the persisted op log gains a
    /// permanent hole that corrupts playback from that point on.
    ///
    /// Cheap and exact: each revision stores its own inverse, captured against
    /// the state it was applied to.
    pub fn rollback_last(&mut self) -> bool {
        let Some(entry) = self.history.pop_back() else {
            return false;
        };
        self.content = compose(&self.content, &entry.undo);
        self.revision -= 1;
        true
    }

    /// Reconstruct the document as it stood at `revision`.
    ///
    /// Walks backwards applying each revision's stored inverse. Returns `None`
    /// when in-memory history no longer reaches that far back — use [`replay`]
    /// over the persisted op log for older points in time.
    pub fn at_revision(&self, revision: u64) -> Option<Delta> {
        if revision == self.revision {
            return Some(self.content.clone());
        }
        if revision > self.revision || revision < self.oldest_retained() {
            return None;
        }
        let mut doc = self.content.clone();
        for entry in self.history.iter().rev() {
            if entry.revision <= revision {
                break;
            }
            doc = compose(&doc, &entry.undo);
        }
        Some(doc)
    }
}

/// Rebuild a document by replaying the first `up_to` ops of an ordered log.
///
/// This is the exact-playback path. The server persists every op, so playback
/// stays accurate long after in-memory history has been trimmed.
pub fn replay(ops: &[Delta], up_to: usize) -> Delta {
    let mut doc = Delta::new();
    for delta in ops.iter().take(up_to) {
        doc = compose(&doc, delta);
    }
    doc
}

#[cfg(test)]
mod tests;
