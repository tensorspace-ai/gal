// A rich-text editor over a Delta document.
//
// The Delta is the source of truth; the DOM is a rendering of it. Local typing
// is read back out of the DOM and diffed into an op, which keeps the fast path
// (plain characters) free of re-rendering and therefore free of caret jumps.
// Anything structural — newlines, paste, formatting, remote ops — is applied to
// the model and then re-rendered.

import { Delta, diffText } from './ot.js';

/** Attributes that map to a wrapping element, innermost first. */
const INLINE_TAGS = [
  ['code', 'code'],
  ['bold', 'strong'],
  ['italic', 'em'],
  ['underline', 'u'],
  ['strike', 's'],
];

/**
 * Only allow link schemes that cannot execute script. An attacker who can type
 * into a shared wave would otherwise be able to plant a `javascript:` link that
 * runs in every other participant's session.
 */
function safeUrl(url) {
  if (typeof url !== 'string') return null;
  const trimmed = url.trim();
  if (/^(https?|mailto):/i.test(trimmed)) return trimmed;
  // Bare domains typed by a user are common; assume https.
  if (/^[\w-]+(\.[\w-]+)+([/?#]|$)/.test(trimmed)) return `https://${trimmed}`;
  return null;
}

/** Build the DOM for one run of text with its formatting. */
function renderRun(text, attributes = {}) {
  let node = document.createTextNode(text);
  for (const [attr, tag] of INLINE_TAGS) {
    if (attributes[attr]) {
      const element = document.createElement(tag);
      element.appendChild(node);
      node = element;
    }
  }
  const href = attributes.link ? safeUrl(attributes.link) : null;
  if (href) {
    const anchor = document.createElement('a');
    anchor.href = href;
    anchor.target = '_blank';
    anchor.rel = 'noopener noreferrer';
    anchor.appendChild(node);
    node = anchor;
  }
  return node;
}

/** Render a whole document into `root`. */
export function renderDelta(root, delta) {
  root.textContent = '';
  const text = delta.toPlainText();

  for (const op of delta.ops) {
    if (typeof op.insert !== 'string') {
      if (op.insert !== undefined) {
        // Embeds are not produced by this build, but render defensively rather
        // than dropping content silently.
        const span = document.createElement('span');
        span.className = 'embed';
        span.textContent = '￼';
        root.appendChild(span);
      }
      continue;
    }
    const lines = op.insert.split('\n');
    lines.forEach((line, i) => {
      if (i > 0) root.appendChild(document.createElement('br'));
      if (line) root.appendChild(renderRun(line, op.attributes));
    });
  }

  // A trailing newline needs an extra <br> to occupy a visible line. It is
  // flagged so it is not counted when reading text back out.
  if (text.endsWith('\n')) {
    const br = document.createElement('br');
    br.dataset.trailing = '1';
    root.appendChild(br);
  }
}

/** Read the plain text of a rendered document back out of the DOM. */
export function readText(root) {
  let text = '';
  const walk = (node) => {
    for (const child of node.childNodes) {
      if (child.nodeType === Node.TEXT_NODE) {
        text += child.data;
      } else if (child.nodeName === 'BR') {
        if (!child.dataset || !child.dataset.trailing) text += '\n';
      } else {
        walk(child);
      }
    }
  };
  walk(root);
  return text;
}

/** Convert a DOM position to a character offset within `root`. */
export function domToIndex(root, node, offset) {
  let index = 0;
  let found = false;

  const walk = (current) => {
    if (found) return;
    for (const child of current.childNodes) {
      if (found) return;
      if (child === node && child.nodeType !== Node.TEXT_NODE) {
        // Selection anchored to an element: offset counts child nodes.
        for (let i = 0; i < offset && i < child.childNodes.length; i += 1) {
          index += lengthOf(child.childNodes[i]);
        }
        found = true;
        return;
      }
      if (child.nodeType === Node.TEXT_NODE) {
        if (child === node) {
          index += Math.min(offset, child.data.length);
          found = true;
          return;
        }
        index += child.data.length;
      } else if (child.nodeName === 'BR') {
        if (!child.dataset || !child.dataset.trailing) index += 1;
      } else {
        walk(child);
      }
    }
  };

  if (node === root) {
    for (let i = 0; i < offset && i < root.childNodes.length; i += 1) {
      index += lengthOf(root.childNodes[i]);
    }
    return index;
  }
  walk(root);
  return index;
}

function lengthOf(node) {
  if (node.nodeType === Node.TEXT_NODE) return node.data.length;
  if (node.nodeName === 'BR') return node.dataset && node.dataset.trailing ? 0 : 1;
  let total = 0;
  for (const child of node.childNodes) total += lengthOf(child);
  return total;
}

/** Convert a character offset within `root` to a DOM position. */
export function indexToDom(root, index) {
  let remaining = index;
  let result = null;

  const walk = (node) => {
    if (result) return;
    for (const child of node.childNodes) {
      if (result) return;
      if (child.nodeType === Node.TEXT_NODE) {
        if (remaining <= child.data.length) {
          result = { node: child, offset: remaining };
          return;
        }
        remaining -= child.data.length;
      } else if (child.nodeName === 'BR') {
        const trailing = child.dataset && child.dataset.trailing;
        if (!trailing) {
          if (remaining === 0) {
            const parent = child.parentNode;
            result = { node: parent, offset: Array.prototype.indexOf.call(parent.childNodes, child) };
            return;
          }
          remaining -= 1;
        }
      } else {
        walk(child);
      }
    }
  };

  walk(root);
  if (result) return result;
  // Past the end: place the caret after the last child.
  return { node: root, offset: root.childNodes.length };
}

/**
 * Compute where text changed between two strings.
 *
 * Returns the index of the change plus how much was removed and inserted, which
 * is what the caller needs to inherit formatting from the character before the
 * caret. `diffText` produces the same edit as a Delta.
 */
function textChange(before, after) {
  const maxAffix = Math.min(before.length, after.length);
  let prefix = 0;
  while (prefix < maxAffix && before[prefix] === after[prefix]) prefix += 1;
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

  return {
    index: prefix,
    removed: before.length - prefix - suffix,
    inserted: after.slice(prefix, after.length - suffix),
  };
}

function isHighSurrogate(code) {
  return code >= 0xd800 && code <= 0xdbff;
}
function isLowSurrogate(code) {
  return code >= 0xdc00 && code <= 0xdfff;
}

/**
 * Binds a contenteditable element to a document.
 *
 * `onChange(delta)` is called with every local edit as an op. The owner is
 * responsible for sending it; call `applyRemote` to fold in other people's ops.
 */
export class Editor {
  constructor(element, doc, { onChange, onSelectionChange, readOnly = false } = {}) {
    this.root = element;
    this.doc = doc; // a Delta
    this.onChange = onChange || (() => {});
    this.onSelectionChange = onSelectionChange || (() => {});
    this.composing = false;
    this.pendingFormat = null;
    this.destroyed = false;

    this.root.contentEditable = readOnly ? 'false' : 'true';
    this.root.spellcheck = true;
    renderDelta(this.root, this.doc);

    this.handlers = {
      beforeinput: (e) => this.onBeforeInput(e),
      input: () => this.onInput(),
      paste: (e) => this.onPaste(e),
      compositionstart: () => {
        this.composing = true;
      },
      compositionend: () => {
        this.composing = false;
        this.onInput();
      },
      keydown: (e) => this.onKeyDown(e),
      keyup: () => this.reportSelection(),
      mouseup: () => this.reportSelection(),
      focus: () => this.reportSelection(),
    };
    for (const [event, handler] of Object.entries(this.handlers)) {
      this.root.addEventListener(event, handler);
    }
  }

  destroy() {
    this.destroyed = true;
    for (const [event, handler] of Object.entries(this.handlers)) {
      this.root.removeEventListener(event, handler);
    }
  }

  // --- selection ------------------------------------------------------

  /** Current selection as `{index, length}`, or null if not in this editor. */
  getSelection() {
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return null;
    const range = selection.getRangeAt(0);
    if (!this.root.contains(range.startContainer)) return null;

    const start = domToIndex(this.root, range.startContainer, range.startOffset);
    const end = range.collapsed
      ? start
      : domToIndex(this.root, range.endContainer, range.endOffset);
    return { index: Math.min(start, end), length: Math.abs(end - start) };
  }

  setSelection(index, length = 0) {
    const selection = window.getSelection();
    if (!selection) return;
    const total = this.doc.toPlainText().length;
    const from = indexToDom(this.root, Math.max(0, Math.min(index, total)));
    const to = indexToDom(this.root, Math.max(0, Math.min(index + length, total)));

    const range = document.createRange();
    try {
      range.setStart(from.node, from.offset);
      range.setEnd(to.node, to.offset);
    } catch {
      return; // DOM moved underneath us; the next render will fix it
    }
    selection.removeAllRanges();
    selection.addRange(range);
  }

  reportSelection() {
    const selection = this.getSelection();
    if (selection) {
      // Moving the caret abandons any pending formatting toggle.
      if (this.lastIndex !== selection.index) this.pendingFormat = null;
      this.lastIndex = selection.index;
      this.onSelectionChange(selection, this.doc.attributesAt(
        Math.max(0, selection.length ? selection.index : selection.index - 1),
        selection.length || 1,
      ));
    }
  }

  // --- input ----------------------------------------------------------

  onKeyDown(event) {
    // Let the toolbar shortcuts through to the document handler.
    if (event.key === 'Enter' && !event.shiftKey && this.onEnter) {
      if (this.onEnter(event) === false) return;
    }
    if (event.key === 'Enter') {
      // Insert a newline ourselves so the browser cannot introduce block
      // elements the model does not know about.
      event.preventDefault();
      this.replaceSelection('\n');
    }
  }

  onBeforeInput(event) {
    if (event.inputType === 'insertParagraph' || event.inputType === 'insertLineBreak') {
      event.preventDefault();
      this.replaceSelection('\n');
    }
  }

  onPaste(event) {
    event.preventDefault();
    const text = (event.clipboardData || window.clipboardData).getData('text/plain');
    if (text) this.replaceSelection(text.replace(/\r\n?/g, '\n'));
  }

  /** Read the DOM after the browser edited it, and turn it into an op. */
  onInput() {
    if (this.composing || this.destroyed) return;

    const before = this.doc.toPlainText();
    const after = readText(this.root);
    if (before === after) return;

    const { index, removed, inserted } = textChange(before, after);
    const delta = new Delta().retain(index).delete(removed);

    if (inserted) {
      // Typed text inherits the formatting of the character it follows, which
      // is what makes typing at the end of a bold run stay bold.
      const attributes =
        this.pendingFormat ||
        (index > 0 ? this.doc.attributesAt(index - 1, 1) : {});
      delta.insert(inserted, attributes);
    }
    this.pendingFormat = null;

    this.doc = this.doc.apply(delta.chop());
    this.onChange(delta.chop());

    // Plain characters land in the DOM correctly on their own. Anything that
    // changes structure or formatting needs a re-render to stay canonical.
    if (inserted.includes('\n') || removed > 0) {
      const caret = index + inserted.length;
      renderDelta(this.root, this.doc);
      this.setSelection(caret);
    }
    this.reportSelection();
  }

  /** Replace the current selection with `text`, as one op. */
  replaceSelection(text) {
    const selection = this.getSelection() || { index: this.doc.toPlainText().length, length: 0 };
    const attributes =
      this.pendingFormat ||
      (selection.index > 0 ? this.doc.attributesAt(selection.index - 1, 1) : {});

    const delta = new Delta()
      .retain(selection.index)
      .delete(selection.length)
      .insert(text, attributes);

    this.pendingFormat = null;
    this.doc = this.doc.apply(delta);
    this.onChange(delta);
    renderDelta(this.root, this.doc);
    this.setSelection(selection.index + text.length);
    this.reportSelection();
  }

  /** Apply or remove a formatting attribute over the selection. */
  format(name, value) {
    const selection = this.getSelection();
    if (!selection) return;

    if (selection.length === 0) {
      // Nothing selected: remember the toggle and apply it to the next
      // characters typed, the way every other editor behaves.
      const current = this.doc.attributesAt(Math.max(0, selection.index - 1), 1);
      this.pendingFormat = { ...current, [name]: value };
      if (value === null) delete this.pendingFormat[name];
      return;
    }

    const delta = new Delta().retain(selection.index).retain(selection.length, { [name]: value });
    this.doc = this.doc.apply(delta);
    this.onChange(delta);
    renderDelta(this.root, this.doc);
    this.setSelection(selection.index, selection.length);
    this.reportSelection();
  }

  /** True when `name` is active across the whole selection. */
  isActive(name) {
    const selection = this.getSelection();
    if (!selection) return false;
    if (this.pendingFormat) return Boolean(this.pendingFormat[name]);
    const at = selection.length
      ? this.doc.attributesAt(selection.index, selection.length)
      : this.doc.attributesAt(Math.max(0, selection.index - 1), 1);
    return Boolean(at[name]);
  }

  /**
   * Fold in a change from another participant, keeping the local caret where
   * the user expects it.
   */
  applyRemote(delta, newDoc) {
    const selection = this.getSelection();
    const hadFocus = this.root.contains(document.activeElement) || document.activeElement === this.root;

    this.doc = newDoc;
    renderDelta(this.root, this.doc);

    if (selection && hadFocus) {
      // Priority keeps the caret from being dragged along by a remote insert
      // that lands exactly where it sits.
      const index = delta.transformPosition(selection.index, true);
      const end = delta.transformPosition(selection.index + selection.length, true);
      this.setSelection(index, Math.max(0, end - index));
    }
  }

  /** Replace the whole document, discarding local state. */
  reset(doc) {
    this.doc = doc;
    this.pendingFormat = null;
    renderDelta(this.root, this.doc);
  }
}
