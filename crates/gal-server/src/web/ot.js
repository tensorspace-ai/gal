// Operational transformation, mirroring the Rust `gal-ot` crate exactly.
//
// Both sides transform the same ops, so any divergence here is a data-corruption
// bug. The algorithms below are equivalents of the Rust implementation, and the
// wire format is shared: an op is `{insert|retain|delete, attributes?}`.
//
// JavaScript strings are already UTF-16, so `String.prototype.length` counts the
// same unit the Rust side measures. That is why the Rust code goes to the
// trouble of counting UTF-16 code units rather than Unicode scalar values.

/** Deep equality for attribute values (JSON scalars, arrays and objects). */
function deepEqual(a, b) {
  if (a === b) return true;
  if (a === null || b === null || typeof a !== 'object' || typeof b !== 'object') return false;
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  return ka.every((k) => deepEqual(a[k], b[k]));
}

function isEmptyAttrs(attributes) {
  return !attributes || Object.keys(attributes).length === 0;
}

/** Normalise to a plain object so callers never deal with undefined. */
function attrs(op) {
  return op && op.attributes ? op.attributes : {};
}

// --- attribute algebra --------------------------------------------------

export function composeAttributes(a, b, keepNull) {
  a = a || {};
  b = b || {};
  const out = {};
  for (const key of Object.keys(b)) {
    if (keepNull || b[key] !== null) out[key] = b[key];
  }
  for (const key of Object.keys(a)) {
    if (!(key in b)) out[key] = a[key];
  }
  return out;
}

export function transformAttributes(a, b, priority) {
  a = a || {};
  b = b || {};
  if (!priority) return { ...b };
  const out = {};
  for (const key of Object.keys(b)) {
    if (!(key in a)) out[key] = b[key];
  }
  return out;
}

export function invertAttributes(attr, base) {
  attr = attr || {};
  base = base || {};
  const out = {};
  // Restore what the change overwrote.
  for (const key of Object.keys(base)) {
    if (!deepEqual(base[key], attr[key]) && key in attr) out[key] = base[key];
  }
  // Remove what the change introduced.
  for (const key of Object.keys(attr)) {
    if (!deepEqual(attr[key], base[key]) && !(key in base)) out[key] = null;
  }
  return out;
}

// --- op helpers ---------------------------------------------------------

export function opLength(op) {
  if (typeof op.delete === 'number') return op.delete;
  if (typeof op.retain === 'number') return op.retain;
  if (typeof op.insert === 'string') return op.insert.length;
  if (op.insert !== undefined) return 1; // embed
  return 0;
}

export function opKind(op) {
  if (typeof op.delete === 'number') return 'delete';
  if (typeof op.retain === 'number') return 'retain';
  return 'insert';
}

/**
 * Advance `units` UTF-16 code units from index `from`, never stopping inside a
 * surrogate pair.
 *
 * A boundary that would split a character is rounded *up* to the next character,
 * matching the Rust engine. Real ops never land there — the browser treats a
 * surrogate pair as atomic for cursor movement, and the server rejects ops that
 * split one — but the two engines must still agree byte-for-byte on malformed
 * input, or a hostile peer could make them diverge.
 */
function advanceUtf16(text, from, units) {
  let consumed = 0;
  let i = from;
  while (i < text.length) {
    if (consumed >= units) return i;
    const code = text.charCodeAt(i);
    let width = 1;
    if (code >= 0xd800 && code <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) width = 2;
    }
    consumed += width;
    i += width;
  }
  return text.length;
}

/** Walks a list of ops, splitting them at arbitrary offsets. */
class OpIterator {
  constructor(ops) {
    this.ops = ops || [];
    this.index = 0;
    this.offset = 0;
    // Index into the current text insert. Tracked separately from `offset`
    // because rounding to a character boundary can move it further than the
    // number of units requested.
    this.textOffset = 0;
  }

  hasNext() {
    return this.peekLength() < Infinity;
  }

  peekLength() {
    const op = this.ops[this.index];
    return op === undefined ? Infinity : opLength(op) - this.offset;
  }

  /** An exhausted iterator behaves like an endless run of untouched document. */
  peekKind() {
    const op = this.ops[this.index];
    return op === undefined ? 'retain' : opKind(op);
  }

  next(length = Infinity) {
    const op = this.ops[this.index];
    if (op === undefined) return { retain: Infinity };

    const total = opLength(op);
    const offset = this.offset;
    const take = Math.min(length, total - offset);

    // Slice before advancing, so the text offset is read at its current value.
    let result;
    if (typeof op.delete === 'number') {
      result = { delete: take };
    } else {
      result = {};
      const a = attrs(op);
      if (!isEmptyAttrs(a)) result.attributes = { ...a };
      if (typeof op.retain === 'number') {
        result.retain = take;
      } else if (typeof op.insert === 'string') {
        const start = this.textOffset;
        const end = advanceUtf16(op.insert, start, take);
        this.textOffset = end;
        result.insert = op.insert.slice(start, end);
      } else {
        result.insert = op.insert; // embeds are atomic
      }
    }

    this.offset += take;
    if (this.offset >= total) {
      this.index += 1;
      this.offset = 0;
      this.textOffset = 0;
    }
    return result;
  }
}

// --- Delta --------------------------------------------------------------

export class Delta {
  constructor(ops) {
    if (ops instanceof Delta) this.ops = ops.ops.slice();
    else if (Array.isArray(ops)) this.ops = ops.slice();
    else if (ops && Array.isArray(ops.ops)) this.ops = ops.ops.slice();
    else this.ops = [];
  }

  static document(text) {
    return new Delta().insert(text);
  }

  insert(value, attributes) {
    if (typeof value === 'string' && value.length === 0) return this;
    const op = { insert: value };
    if (!isEmptyAttrs(attributes)) op.attributes = { ...attributes };
    return this.push(op);
  }

  retain(length, attributes) {
    if (length <= 0) return this;
    const op = { retain: length };
    if (!isEmptyAttrs(attributes)) op.attributes = { ...attributes };
    return this.push(op);
  }

  delete(length) {
    if (length <= 0) return this;
    return this.push({ delete: length });
  }

  /** Append an op, merging it into the previous one where possible. */
  push(newOp) {
    // Zero-length ops carry no meaning and must not reach the wire. Embeds are
    // length 1, so this never discards one.
    if (opLength(newOp) === 0) return this;

    let index = this.ops.length;
    if (index === 0) {
      this.ops.push(newOp);
      return this;
    }
    let last = this.ops[index - 1];

    // Merge adjacent deletes.
    if (typeof newOp.delete === 'number' && typeof last.delete === 'number') {
      this.ops[index - 1] = { delete: last.delete + newOp.delete };
      return this;
    }

    // Inserting where a delete also happens is order-agnostic; canonicalise so
    // the insert comes first and equivalent deltas compare equal.
    if (typeof last.delete === 'number' && newOp.insert !== undefined) {
      index -= 1;
      last = this.ops[index - 1];
      if (last === undefined) {
        this.ops.unshift(newOp);
        return this;
      }
    }

    if (deepEqual(attrs(newOp), attrs(last))) {
      if (typeof newOp.insert === 'string' && typeof last.insert === 'string') {
        const merged = { insert: last.insert + newOp.insert };
        if (!isEmptyAttrs(attrs(newOp))) merged.attributes = { ...attrs(newOp) };
        this.ops[index - 1] = merged;
        return this;
      }
      if (typeof newOp.retain === 'number' && typeof last.retain === 'number') {
        const merged = { retain: last.retain + newOp.retain };
        if (!isEmptyAttrs(attrs(newOp))) merged.attributes = { ...attrs(newOp) };
        this.ops[index - 1] = merged;
        return this;
      }
    }

    this.ops.splice(index, 0, newOp);
    return this;
  }

  /** Drop a trailing bare retain; it is a no-op. */
  chop() {
    const last = this.ops[this.ops.length - 1];
    if (last && typeof last.retain === 'number' && isEmptyAttrs(attrs(last))) {
      this.ops.pop();
    }
    return this;
  }

  get length() {
    return this.ops.reduce((sum, op) => sum + opLength(op), 0);
  }

  /** Length of the document this delta is written against. */
  baseLength() {
    return this.ops.reduce(
      (sum, op) => sum + (op.insert === undefined ? opLength(op) : 0),
      0,
    );
  }

  /** Length of the document this delta produces. */
  targetLength() {
    return this.ops.reduce(
      (sum, op) => sum + (typeof op.delete === 'number' ? 0 : opLength(op)),
      0,
    );
  }

  isEmpty() {
    return this.ops.length === 0;
  }

  /** Plain text, with each embed collapsed to one placeholder. */
  toPlainText() {
    return this.ops
      .map((op) => {
        if (typeof op.insert === 'string') return op.insert;
        if (op.insert !== undefined) return '￼';
        return '';
      })
      .join('');
  }

  /** Extract `[start, end)` of a document delta. */
  slice(start = 0, end = Infinity) {
    const out = new Delta();
    const iter = new OpIterator(this.ops);
    let index = 0;
    while (index < end && iter.hasNext()) {
      let nextOp;
      if (index < start) {
        nextOp = iter.next(start - index);
      } else {
        nextOp = iter.next(end - index);
        out.push(nextOp);
      }
      index += opLength(nextOp);
    }
    return out;
  }

  compose(other) {
    return compose(this, other);
  }

  apply(change) {
    return compose(this, change);
  }

  transform(other, priority) {
    return transform(this, other, priority);
  }

  transformPosition(index, priority) {
    return transformPosition(this, index, priority);
  }

  invert(base) {
    return invert(this, base);
  }

  /**
   * Formatting shared across `[index, index+length)`, for lighting up the
   * toolbar. A zero-length selection reports the formatting at the caret.
   */
  attributesAt(index, length) {
    const slice = this.slice(index, index + Math.max(length, 1));
    let result = null;
    for (const op of slice.ops) {
      if (op.insert === undefined) continue;
      const a = attrs(op);
      if (result === null) {
        result = { ...a };
      } else {
        for (const key of Object.keys(result)) {
          if (!deepEqual(result[key], a[key])) delete result[key];
        }
      }
    }
    return result || {};
  }
}

// --- core algorithms ----------------------------------------------------

/** Sequential composition: the single delta equivalent to `a` then `b`. */
export function compose(a, b) {
  const ia = new OpIterator(a.ops);
  const ib = new OpIterator(b.ops);
  const out = new Delta();

  while (ia.hasNext() || ib.hasNext()) {
    if (ib.peekKind() === 'insert') {
      out.push(ib.next());
    } else if (ia.peekKind() === 'delete') {
      out.push(ia.next());
    } else {
      const length = Math.min(ia.peekLength(), ib.peekLength());
      const opA = ia.next(length);
      const opB = ib.next(length);

      if (typeof opB.retain === 'number') {
        const newOp = {};
        // Take the span from `length`, not from opA: an exhausted iterator
        // reports an infinite retain, which must not leak into the result.
        if (typeof opA.retain === 'number') newOp.retain = length;
        else newOp.insert = opA.insert;

        const merged = composeAttributes(
          attrs(opA),
          attrs(opB),
          typeof opA.retain === 'number',
        );
        if (!isEmptyAttrs(merged)) newOp.attributes = merged;
        out.push(newOp);
      } else if (typeof opB.delete === 'number' && typeof opA.retain === 'number') {
        out.push(opB);
      }
      // Remaining case: b deletes what a inserted, so both disappear.
    }
  }
  return out.chop();
}

/**
 * Rewrite `b` — written against the same base as `a` — so it can be applied
 * after `a`. `priority` treats `a` as having happened first, which only matters
 * for breaking ties between two inserts at the same position.
 */
export function transform(a, b, priority) {
  const ia = new OpIterator(a.ops);
  const ib = new OpIterator(b.ops);
  const out = new Delta();

  while (ia.hasNext() || ib.hasNext()) {
    if (ia.peekKind() === 'insert' && (priority || ib.peekKind() !== 'insert')) {
      out.retain(opLength(ia.next()));
    } else if (ib.peekKind() === 'insert') {
      out.push(ib.next());
    } else {
      const length = Math.min(ia.peekLength(), ib.peekLength());
      const opA = ia.next(length);
      const opB = ib.next(length);

      if (typeof opA.delete === 'number') {
        // a already removed this content, so b's op has nothing to act on.
        continue;
      } else if (typeof opB.delete === 'number') {
        out.push(opB);
      } else {
        out.retain(length, transformAttributes(attrs(opA), attrs(opB), priority));
      }
    }
  }
  return out.chop();
}

/**
 * Map a cursor offset across a delta. `priority` keeps a cursor sitting exactly
 * at an insertion point from being dragged along, which is what you want for
 * other people's carets but not your own.
 */
export function transformPosition(delta, index, priority) {
  const iter = new OpIterator(delta.ops);
  let offset = 0;

  while (iter.hasNext() && offset <= index) {
    const length = iter.peekLength();
    const kind = iter.peekKind();
    iter.next();

    if (kind === 'delete') {
      index -= Math.min(length, index - offset);
      continue;
    }
    if (kind === 'insert' && (offset < index || !priority)) {
      index += length;
    }
    offset += length;
  }
  return index;
}

/** The delta that undoes `change` when applied to `base`. */
export function invert(change, base) {
  const inverted = new Delta();
  let baseIndex = 0;

  for (const op of change.ops) {
    if (op.insert !== undefined) {
      inverted.delete(opLength(op));
    } else if (typeof op.retain === 'number' && isEmptyAttrs(attrs(op))) {
      inverted.retain(op.retain);
      baseIndex += op.retain;
    } else {
      const length = opLength(op);
      const slice = base.slice(baseIndex, baseIndex + length);
      for (const baseOp of slice.ops) {
        if (typeof op.delete === 'number') {
          inverted.push(baseOp);
        } else {
          inverted.retain(opLength(baseOp), invertAttributes(attrs(op), attrs(baseOp)));
        }
      }
      baseIndex += length;
    }
  }
  return inverted.chop();
}

/**
 * Compute a delta turning `before` into `after` using a common prefix/suffix
 * scan. That covers how text is actually edited — typing, pasting, deleting a
 * selection — in linear time.
 */
export function diffText(before, after) {
  if (before === after) return new Delta();

  const maxAffix = Math.min(before.length, after.length);

  let prefix = 0;
  while (prefix < maxAffix && before[prefix] === after[prefix]) prefix += 1;
  // Never split a surrogate pair: the server rejects ops that would.
  if (prefix > 0 && isLowSurrogate(before.charCodeAt(prefix))) prefix -= 1;

  let suffix = 0;
  const maxSuffix = maxAffix - prefix;
  while (
    suffix < maxSuffix &&
    before[before.length - 1 - suffix] === after[after.length - 1 - suffix]
  ) {
    suffix += 1;
  }
  if (suffix > 0 && isHighSurrogate(before.charCodeAt(before.length - suffix))) suffix -= 1;

  const delta = new Delta();
  delta.retain(prefix);
  delta.delete(before.length - prefix - suffix);
  delta.insert(after.slice(prefix, after.length - suffix));
  return delta.chop();
}

function isHighSurrogate(code) {
  return code >= 0xd800 && code <= 0xdbff;
}

function isLowSurrogate(code) {
  return code >= 0xdc00 && code <= 0xdfff;
}
