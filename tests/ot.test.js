// Conformance and property tests for the browser OT engine.
//
// Run: ./run-tests.sh   (or: node tests/ot.test.js)
//
// The conformance half replays vectors produced by the Rust engine and asserts
// the JavaScript engine returns byte-identical results. The property half
// re-checks the convergence law directly in JavaScript, so a break is caught
// even without regenerated vectors.

import { readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import {
  Delta,
  compose,
  transform,
  transformPosition,
  invert,
  diffText,
} from '../crates/gal-server/src/web/ot.js';

const here = dirname(fileURLToPath(import.meta.url));

let passed = 0;
let failed = 0;

function check(name, condition, detail) {
  if (condition) {
    passed += 1;
  } else {
    failed += 1;
    console.error(`FAIL  ${name}`);
    if (detail) console.error(`      ${detail}`);
  }
}

/** Compare against the Rust encoding, which omits empty attribute maps. */
function normalise(delta) {
  return JSON.stringify({
    ops: (delta.ops || []).map((op) => {
      const out = {};
      if (op.insert !== undefined) out.insert = op.insert;
      if (typeof op.retain === 'number') out.retain = op.retain;
      if (typeof op.delete === 'number') out.delete = op.delete;
      if (op.attributes && Object.keys(op.attributes).length > 0) {
        // Sort keys: the Rust side uses a BTreeMap.
        out.attributes = Object.fromEntries(
          Object.keys(op.attributes).sort().map((k) => [k, op.attributes[k]]),
        );
      }
      return out;
    }),
  });
}

// --- conformance against the Rust engine --------------------------------

const vectorPath = join(here, 'vectors.json');
if (!existsSync(vectorPath)) {
  console.error(
    'vectors.json missing — generate it with:\n' +
      '  cargo run -p gal-ot --example gen_vectors > crates/gal-server/src/web/vectors.json',
  );
  process.exit(1);
}

const vectors = JSON.parse(readFileSync(vectorPath, 'utf8'));
let mismatch = null;

for (const [i, c] of vectors.entries()) {
  const doc = new Delta(c.doc);
  const a = new Delta(c.a);
  const b = new Delta(c.b);
  const want = c.expected;

  const results = {
    composeDocA: normalise(compose(doc, a)),
    composeAB: normalise(compose(a, b)),
    transformABTrue: normalise(transform(a, b, true)),
    transformBAFalse: normalise(transform(b, a, false)),
    invertA: normalise(invert(a, doc)),
  };

  for (const [key, got] of Object.entries(results)) {
    const expected = normalise(new Delta(want[key]));
    if (got !== expected) {
      mismatch = mismatch || { i, key, got, expected, c };
    }
  }

  if (transformPosition(a, c.position, true) !== want.positionTrue) {
    mismatch = mismatch || { i, key: 'positionTrue' };
  }
  if (transformPosition(a, c.position, false) !== want.positionFalse) {
    mismatch = mismatch || { i, key: 'positionFalse' };
  }
}

check(
  `conformance with the Rust engine across ${vectors.length} vectors`,
  mismatch === null,
  mismatch
    ? `case ${mismatch.i} field ${mismatch.key}\n      rust: ${mismatch.expected}\n      js:   ${mismatch.got}`
    : null,
);

// --- convergence property, checked natively -----------------------------

function xorshift(seed) {
  let x = BigInt(seed);
  const mask = (1n << 64n) - 1n;
  return () => {
    x ^= (x >> 12n) & mask;
    x = (x ^ ((x << 25n) & mask)) & mask;
    x ^= x >> 27n;
    return Number(((x * 0x2545f4914f6cdd1dn) & mask) >> 33n);
  };
}

function boundaries(text) {
  const offsets = [0];
  for (const ch of text) offsets.push(offsets[offsets.length - 1] + ch.length);
  return offsets;
}

function randomDelta(rand, bounds) {
  const last = bounds.length - 1;
  const delta = new Delta();
  let at = 0;
  while (at < last) {
    const end = at + 1 + (rand() % (last - at));
    const span = bounds[end] - bounds[at];
    switch (rand() % 4) {
      case 0:
        delta.retain(span);
        at = end;
        break;
      case 1:
        delta.delete(span);
        at = end;
        break;
      case 2: {
        const words = ['cat', '🌊', 'xyz', 'the ', 'é'];
        delta.insert(words[rand() % words.length]);
        break;
      }
      default: {
        const keys = ['bold', 'italic', 'link'];
        const value = rand() % 3 === 0 ? null : true;
        delta.retain(span, { [keys[rand() % keys.length]]: value });
        at = end;
        break;
      }
    }
    if (rand() % 8 === 0) break;
  }
  return delta.chop();
}

const rand = xorshift('0x9E3779B97F4A7C15');
const docs = [
  Delta.document('Hello world'),
  Delta.document('The quick brown fox jumps over the lazy dog'),
  Delta.document('a🌊b🌊c émoji'),
];

let diverged = null;
let invertFailed = null;
for (let round = 0; round < 3000 && !diverged; round += 1) {
  const doc = docs[rand() % docs.length];
  const bounds = boundaries(doc.toPlainText());
  const a = randomDelta(rand, bounds);
  const b = randomDelta(rand, bounds);

  const aThenB = normalise(compose(compose(doc, a), transform(a, b, true)));
  const bThenA = normalise(compose(compose(doc, b), transform(b, a, false)));
  if (aThenB !== bThenA) diverged = { round, a, b, aThenB, bThenA };

  const restored = normalise(compose(compose(doc, a), invert(a, doc)));
  if (restored !== normalise(doc)) invertFailed = { round, a };
}

check(
  'convergence property over 3000 random operation pairs',
  diverged === null,
  diverged
    ? `round ${diverged.round}\n      a→b: ${diverged.aThenB}\n      b→a: ${diverged.bThenA}`
    : null,
);
check(
  'invert undoes any change against its base document',
  invertFailed === null,
  invertFailed ? `round ${invertFailed.round}` : null,
);

// --- targeted behaviour -------------------------------------------------

check(
  'diff handles append, replace, clear and astral characters',
  ['hello→hello world', 'hello→hi', 'hello→', '→hello', 'a🌊b→a🌊🎉b', 'a🌊b→ab']
    .map((c) => c.split('→'))
    .every(([before, after]) => Delta.document(before).apply(diffText(before, after)).toPlainText() === after),
);

check(
  'diff never splits a surrogate pair',
  (() => {
    const d = diffText('a🌊b', 'ab');
    return d.ops.every((op) => typeof op.delete !== 'number' || op.delete === 2);
  })(),
);

check('empty diff produces no ops', diffText('same', 'same').isEmpty());

check(
  'attributesAt reports only formatting shared by the whole range',
  (() => {
    const doc = new Delta().insert('ab', { bold: true }).insert('cd');
    return (
      doc.attributesAt(0, 2).bold === true &&
      doc.attributesAt(0, 4).bold === undefined &&
      doc.attributesAt(2, 2).bold === undefined
    );
  })(),
);

check(
  'insert tie-breaking matches the documented priority rule',
  normalise(transform(new Delta().insert('a'), new Delta().insert('b'), true)) ===
    normalise(new Delta().retain(1).insert('b')) &&
    normalise(transform(new Delta().insert('a'), new Delta().insert('b'), false)) ===
      normalise(new Delta().insert('b')),
);

check(
  'a cursor inside deleted text collapses to the deletion point',
  transformPosition(new Delta().retain(2).delete(5), 4, false) === 2,
);

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
