// A rich-text editor over a Delta document.
//
// The Delta is the source of truth; the DOM is a rendering of it. Local typing
// is read back out of the DOM and diffed into an op, which keeps the fast path
// (plain characters) free of re-rendering and therefore free of caret jumps.
// Anything structural — newlines, paste, formatting, remote ops — is applied to
// the model and then re-rendered.

import { Delta, diffText } from './ot.js';
import { fileSize, icon, ICONS } from './ui.js';

/** Attributes that map to a wrapping element, innermost first. */
const INLINE_TAGS = [
  ['code', 'code'],
  ['bold', 'strong'],
  ['italic', 'em'],
  ['underline', 'u'],
  ['strike', 's'],
];

/**
 * What an embed contributes to the document's text.
 *
 * U+FFFC OBJECT REPLACEMENT CHARACTER, one UTF-16 code unit, which is exactly
 * what an embed measures in both OT engines. The DOM walkers below hand this
 * back for an embed node instead of descending into it, so an attachment is a
 * single indivisible character as far as every offset in this file is
 * concerned — including the ones sent to the server.
 */
const EMBED_CHAR = '￼';

/** Is this node an embed — an attachment, or anything else atomic? */
function isEmbed(node) {
  return node.nodeType === Node.ELEMENT_NODE && node.dataset && node.dataset.embed === '1';
}

/**
 * Build the DOM for one embed.
 *
 * `contenteditable="false"` is what makes it atomic: the browser will move the
 * caret over it and delete it whole, but never put a caret inside it, so its
 * internals can be as elaborate as they like without the text-diffing path
 * ever seeing them.
 */
function renderEmbed(value) {
  const node = document.createElement('span');
  node.className = 'embed';
  node.contentEditable = 'false';
  node.dataset.embed = '1';
  // The browser will happily drag one of these to another point in the same
  // message, which moves the *element* behind the model's back. onInput has a
  // backstop for that; this stops it being offered in the first place.
  node.draggable = false;

  const attachment = value && typeof value === 'object' ? value.attachment : null;
  if (!attachment || typeof attachment.id !== 'string') {
    // Something a later version knows how to draw and this one does not. Show
    // that it is there rather than silently rendering nothing.
    node.textContent = EMBED_CHAR;
    return node;
  }

  const href = `/api/attachments/${encodeURIComponent(attachment.id)}`;
  const name = typeof attachment.name === 'string' ? attachment.name : 'file';

  if (typeof attachment.mime === 'string' && attachment.mime.startsWith('image/')) {
    const link = document.createElement('a');
    link.href = href;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    node.classList.add('embed-image');

    const image = document.createElement('img');
    image.src = href;
    image.alt = name;
    image.loading = 'lazy';
    // A file can pass the server's magic-byte check and still be a picture no
    // decoder will take, and an attachment can outlive the wave it came from.
    // A broken-image icon with the filename spilling out from under it is
    // worse than the download it turns into here.
    image.addEventListener('error', () => {
      node.classList.replace('embed-image', 'embed-file');
      node.textContent = '';
      node.appendChild(fileChip(href, name, attachment.size));
    });
    link.appendChild(image);
    node.appendChild(link);
    return node;
  }

  node.classList.add('embed-file');
  node.appendChild(fileChip(href, name, attachment.size));
  return node;
}

/** A named, downloadable file, for anything that is not shown in place. */
function fileChip(href, name, size) {
  const link = document.createElement('a');
  link.href = href;
  link.download = name;
  link.appendChild(icon(ICONS.paperclip, { size: 14 }));

  const label = document.createElement('span');
  label.className = 'embed-name';
  label.textContent = name;
  link.appendChild(label);

  if (typeof size === 'number') {
    const bytes = document.createElement('span');
    bytes.className = 'embed-size';
    bytes.textContent = fileSize(size);
    link.appendChild(bytes);
  }
  return link;
}

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
      if (op.insert !== undefined) root.appendChild(renderEmbed(op.insert));
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
      } else if (isEmbed(child)) {
        // One character, whatever it contains. Descending would read an
        // image's alt text or a filename back as if it were typed.
        text += EMBED_CHAR;
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
      if (isEmbed(child)) {
        // A selection that lands anywhere inside an embed is at its start:
        // there are no positions within one.
        if (child === node || child.contains(node)) {
          found = true;
          return;
        }
        index += 1;
        continue;
      }
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
  if (isEmbed(node)) return 1;
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
      } else if (isEmbed(child)) {
        if (remaining === 0) {
          const parent = child.parentNode;
          result = { node: parent, offset: Array.prototype.indexOf.call(parent.childNodes, child) };
          return;
        }
        remaining -= 1;
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
  constructor(element, doc, { onChange, onSelectionChange, onFiles, readOnly = false } = {}) {
    this.root = element;
    this.doc = doc; // a Delta
    this.onChange = onChange || (() => {});
    this.onSelectionChange = onSelectionChange || (() => {});
    /// Called with a FileList's worth of files dropped or pasted in. The owner
    /// uploads them and calls `insertEmbed`; the editor has no idea what a
    /// server is.
    this.onFiles = onFiles || null;
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
      dragover: (e) => this.onDragOver(e),
      dragleave: () => this.root.classList.remove('drop-target'),
      drop: (e) => this.onDrop(e),
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
    const data = event.clipboardData || window.clipboardData;
    const text = data ? data.getData('text/plain') : '';
    const files = data && data.files ? Array.from(data.files) : [];

    // Text wins when the clipboard has both. A screenshot arrives as a file
    // and nothing else, but copying a spreadsheet range or a region of a page
    // puts a bitmap *beside* the text — taking the file there would silently
    // throw away what the person actually copied.
    if (!text && files.length > 0 && this.onFiles) {
      this.onFiles(files, this.getSelection());
      return;
    }
    if (text) this.replaceSelection(text.replace(/\r\n?/g, '\n'));
  }

  /** Only claim a drag that is actually carrying files. */
  onDragOver(event) {
    if (!this.onFiles || !event.dataTransfer) return;
    if (!Array.from(event.dataTransfer.types || []).includes('Files')) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
    this.root.classList.add('drop-target');
  }

  onDrop(event) {
    this.root.classList.remove('drop-target');
    if (!this.onFiles || !event.dataTransfer) return;
    const files = Array.from(event.dataTransfer.files || []);
    if (files.length === 0) return;
    // Only now: letting the browser handle a file drop into a contenteditable
    // splices a copy of it into the DOM that the model knows nothing about.
    event.preventDefault();
    this.onFiles(files, this.pointToSelection(event.clientX, event.clientY));
  }

  /**
   * Where a pointer landed, as a document offset.
   *
   * A file dropped into the middle of a paragraph should go there, not at the
   * end. Returns null when the browser will not say, which puts it at the
   * caret or the end — the old behaviour, and a reasonable one.
   */
  pointToSelection(x, y) {
    let node = null;
    let offset = 0;
    if (document.caretPositionFromPoint) {
      const position = document.caretPositionFromPoint(x, y);
      if (position) {
        node = position.offsetNode;
        offset = position.offset;
      }
    } else if (document.caretRangeFromPoint) {
      const range = document.caretRangeFromPoint(x, y);
      if (range) {
        node = range.startContainer;
        offset = range.startOffset;
      }
    }
    if (!node || !this.root.contains(node)) return null;
    return { index: domToIndex(this.root, node, offset), length: 0 };
  }

  /** Read the DOM after the browser edited it, and turn it into an op. */
  onInput() {
    if (this.composing || this.destroyed) return;

    const before = this.doc.toPlainText();
    const after = readText(this.root);
    if (before === after) return;

    const { index, removed, inserted } = textChange(before, after);

    // An embed only ever enters the document through `insertEmbed`, which
    // re-renders rather than going through this path — so U+FFFC turning up in
    // *typed* text means the browser moved or copied an embed element itself,
    // and the diff is about to replace an attachment with a literal
    // replacement character. That op would be broadcast, and everyone's copy
    // of the picture would become an empty box. Put the DOM back instead.
    if (inserted.includes(EMBED_CHAR)) {
      const caret = this.getSelection();
      renderDelta(this.root, this.doc);
      if (caret) this.setSelection(caret.index, caret.length);
      return;
    }

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

  /**
   * Insert an embed at `at`, or at the caret, as one op.
   *
   * `at` exists because uploading takes a moment and opening a file picker
   * takes the focus: the caret is captured when the file is chosen, not when
   * the bytes come back.
   */
  insertEmbed(value, at = null) {
    const total = this.doc.toPlainText().length;
    const selection = at || this.getSelection() || { index: total, length: 0 };
    // `at` was measured before the upload started, and the document may have
    // been emptied or rewritten since. An op that retains past the end is not
    // a document any more: the server refuses the whole message, and the
    // draft and the file that was just uploaded both go with it.
    const index = Math.max(0, Math.min(selection.index, total));
    const length = Math.max(0, Math.min(selection.length, total - index));

    const delta = new Delta().retain(index).delete(length).insert(value);

    this.pendingFormat = null;
    this.doc = this.doc.apply(delta);
    this.onChange(delta);
    renderDelta(this.root, this.doc);
    // An embed is one unit wide, whatever it draws.
    this.setSelection(index + 1);
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
