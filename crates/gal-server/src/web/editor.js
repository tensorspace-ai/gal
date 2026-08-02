// A rich-text editor over a Delta document.
//
// The Delta is the source of truth; the DOM is a rendering of it. Local typing
// is read back out of the DOM and diffed into an op, which keeps the fast path
// (plain characters) free of re-rendering and therefore free of caret jumps.
// Anything structural — newlines, paste, formatting, remote ops — is applied to
// the model and then re-rendered.

import { compose, Delta, diffText, invert, transform } from './ot.js';
import { fileSize, icon, ICONS } from './ui.js';

/** Steps of undo kept per message. */
const MAX_UNDO = 100;

/** Consecutive typing within this long is undone as one step. */
const COALESCE_MS = 900;

/**
 * Rebase an undo or redo stack over somebody else's change.
 *
 * This is the reason undo cannot just be a list of inverse ops. Every entry was
 * written against a document that has since moved: while it sat on the stack,
 * other people were editing the same message, and applying it unchanged would
 * delete or insert at offsets that now mean something else. Undo in a
 * collaborative editor also has to be *local* — it must take back your last
 * edit, not the last edit — which is the same requirement stated differently.
 *
 * The threading is what makes it right for entries deeper than the top one.
 * Each is transformed over the remote change as it currently stands, and the
 * remote change is then itself transformed over that entry before moving to the
 * next, so every step is compared against the document it actually applied to.
 * This is the algorithm Quill uses, for the same reason.
 */
function rebaseStack(stack, remote) {
  let against = remote;
  for (let i = stack.length - 1; i >= 0; i -= 1) {
    const entry = stack[i];
    stack[i] = { ...entry, delta: transform(against, entry.delta, true) };
    against = transform(entry.delta, against, false);
  }
}

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

/**
 * The attribute that anchors a range of text to a comment thread.
 *
 * Must match `COMMENT_ATTRIBUTE` in `gal-core/src/model.rs`. It is only ever an
 * attribute, never anything the OT engines treat specially, which is the point:
 * an anchor is transformed by exactly the same code that transforms bold.
 */
export const COMMENT_ATTR = 'comment';

/**
 * A mention, carrying the id of the person named.
 *
 * An attribute rather than an embed, for the same reason a comment anchor is
 * one: it is transformed by the code that already transforms bold, so a mention
 * survives everything anyone types around it without a line of its own. Keeping
 * the id — rather than trusting the text — is what lets "did this name me?"
 * stay true after somebody renames themselves or edits the words.
 *
 * Must match `MENTION_ATTRIBUTE` in `gal-core/src/model.rs`.
 */
export const MENTION_ATTR = 'mention';

/** Who this delta names, as a set of user ids. */
export function mentionedUsers(delta) {
  const ids = new Set();
  for (const op of delta.ops || []) {
    const id = op.attributes && op.attributes[MENTION_ATTR];
    if (typeof id === 'string' && id) ids.add(id);
  }
  return ids;
}

/**
 * Formatting that text typed *after* a run should inherit from it.
 *
 * Everything except the comment anchor. Typing at the end of a bold word should
 * stay bold, but typing at the end of a commented sentence must not silently
 * pull the new words into someone else's comment: a comment is a range a person
 * chose, not a style that spreads.
 */
function inheritable(attributes) {
  if (!attributes) return attributes;
  if (attributes[COMMENT_ATTR] === undefined && attributes[MENTION_ATTR] === undefined) {
    return attributes;
  }
  const inherited = { ...attributes };
  delete inherited[COMMENT_ATTR];
  // A mention is a name, not a style. Typing after one must not extend it, or
  // the rest of the sentence quietly becomes part of who was named.
  delete inherited[MENTION_ATTR];
  return inherited;
}

/**
 * Put the anchor back when the text is being typed *inside* a commented run.
 *
 * `inheritable` strips anchors so that typing at the end of a commented phrase
 * does not drag the next words into it. Strictly inside, the opposite is
 * wanted: leaving the anchor off would punch a hole through the middle of the
 * highlight and split one anchor into two runs, which is how a comment ends up
 * pointing at half the phrase it was made about.
 *
 * Inside means the character before the insertion and the character after the
 * text being replaced both carry the same thread. `index`/`removed` are in the
 * document as it was before the change.
 */
function interiorComment(doc, attributes, index, removed) {
  const total = doc.toPlainText().length;
  if (index === 0 || index + removed >= total) return attributes;
  const before = doc.attributesAt(index - 1, 1)[COMMENT_ATTR];
  const after = doc.attributesAt(index + removed, 1)[COMMENT_ATTR];
  if (!before || before !== after) return attributes;
  return { ...attributes, [COMMENT_ATTR]: before };
}

/**
 * Every range of `delta` that is anchored to a comment, as
 * `{ id, index, length }` in UTF-16 code units.
 *
 * Derived from the document each time rather than stored: the document is the
 * only place an anchor lives, so this can never drift from it. Runs that touch
 * and share an id are one range — an edit inside a commented sentence splits
 * the op without splitting the comment.
 */
export function commentRanges(delta) {
  const ranges = [];
  let index = 0;
  for (const op of delta.ops) {
    if (op.insert === undefined) continue;
    const length = typeof op.insert === 'string' ? op.insert.length : 1;
    const id = op.attributes && op.attributes[COMMENT_ATTR];
    if (typeof id === 'string' && id) {
      const last = ranges[ranges.length - 1];
      if (last && last.id === id && last.index + last.length === index) {
        last.length += length;
      } else {
        ranges.push({ id, index, length });
      }
    }
    index += length;
  }
  return ranges;
}

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

/**
 * Who is reading, so a mention of them can be drawn differently.
 *
 * Set once at sign-in. renderRun is called for every run of every message on
 * every render, and threading an identity through all of it to answer one
 * question would be worse than this.
 */
const CURRENT_USER = { id: null };

export function setCurrentUser(id) {
  CURRENT_USER.id = id;
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
  const mention = attributes[MENTION_ATTR];
  if (typeof mention === 'string' && mention) {
    const named = document.createElement('span');
    named.className = mention === CURRENT_USER.id ? 'mention mention-you' : 'mention';
    named.dataset.user = mention;
    named.appendChild(node);
    node = named;
  }

  // Outermost, so the highlight covers the run whatever else it is wearing. A
  // plain <span> is transparent to every walker below, so it costs no offsets.
  const comment = attributes[COMMENT_ATTR];
  if (typeof comment === 'string' && comment) {
    const marked = document.createElement('span');
    marked.className = 'commented';
    marked.dataset.comment = comment;
    marked.appendChild(node);
    node = marked;
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
    /// Local edit history, as ops to apply rather than states to restore: a
    /// state would overwrite whatever anyone else had written in the meantime.
    this.history = { undo: [], redo: [] };
    /// Set while an undo is being applied, so it is not recorded as a new edit.
    this.replaying = false;

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

  // --- history --------------------------------------------------------

  /**
   * Apply a local change: update the model, remember how to take it back, and
   * hand it to the owner to send.
   *
   * Every local edit goes through here, which is the only way undo can be
   * complete — a path that updated `doc` and called `onChange` directly would
   * be an edit that cannot be undone, and the person doing it would find that
   * out at the worst moment.
   */
  applyLocal(delta, { coalesce = false, selection = null } = {}) {
    const before = this.doc;
    this.doc = this.doc.apply(delta);
    this.remember(delta, before, coalesce, selection);
    this.onChange(delta);
  }

  remember(delta, before, coalesce, selection) {
    if (this.replaying) return; // an undo is not itself a new thing to undo

    const undo = invert(delta, before);
    const now = Date.now();
    const top = this.history.undo[this.history.undo.length - 1];

    // Typing a word is one step, not one per keystroke. Composing in the *other
    // order* than it reads: to take back op1 then op2, you undo op2 first.
    if (coalesce && top && top.coalescing && now - top.at < COALESCE_MS) {
      top.delta = compose(undo, top.delta);
      top.at = now;
    } else {
      this.history.undo.push({ delta: undo, at: now, coalescing: coalesce, selection });
      if (this.history.undo.length > MAX_UNDO) this.history.undo.shift();
    }
    // A fresh edit forks the timeline; there is no future to redo into.
    this.history.redo.length = 0;
  }

  undo() {
    return this.step('undo', 'redo');
  }

  redo() {
    return this.step('redo', 'undo');
  }

  step(from, to) {
    const entry = this.history[from].pop();
    if (!entry) return false;

    const before = this.doc;
    // Derived here rather than stored, so the two directions cannot drift apart
    // as the stacks are rebased over other people's edits.
    const reverse = invert(entry.delta, before);
    const selectionBefore = this.getSelection();

    this.replaying = true;
    try {
      this.doc = before.apply(entry.delta);
      this.onChange(entry.delta);
    } finally {
      this.replaying = false;
    }

    this.history[to].push({
      delta: reverse,
      at: Date.now(),
      coalescing: false,
      selection: selectionBefore,
    });

    renderDelta(this.root, this.doc);
    // Back to where the edit was made. Undoing something off-screen and being
    // left looking at where you happened to be is disorienting.
    const target = entry.selection;
    const length = this.doc.toPlainText().length;
    if (target) {
      this.setSelection(Math.min(target.index, length), 0);
    }
    this.reportSelection();
    return true;
  }

  // --- input ----------------------------------------------------------

  onKeyDown(event) {
    // Undo and redo, before anything else looks at the key. The browser's own
    // contenteditable history is worse than useless here: renderDelta replaces
    // the DOM wholesale on every structural edit and on every remote op, so the
    // native stack is repeatedly invalidated and would undo into states this
    // document was never in.
    const accel = event.metaKey || event.ctrlKey;
    if (accel && (event.key === 'z' || event.key === 'Z')) {
      event.preventDefault();
      if (event.shiftKey) this.redo();
      else this.undo();
      return;
    }
    if (accel && (event.key === 'y' || event.key === 'Y')) {
      event.preventDefault();
      this.redo();
      return;
    }

    // A picker open over the caret gets first refusal on the keys it uses —
    // Enter above all, which would otherwise send the message instead of
    // choosing the person being named.
    if (this.onPickerKey && this.onPickerKey(event)) {
      event.preventDefault();
      return;
    }

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
      return;
    }
    // Undo reached by a route that is not a keystroke — the Edit menu, a
    // trackpad gesture, the Android keyboard. Refused and redirected, because
    // the browser's own history is of a DOM that renderDelta has replaced
    // wholesale more than once.
    if (event.inputType === 'historyUndo') {
      event.preventDefault();
      this.undo();
      return;
    }
    if (event.inputType === 'historyRedo') {
      event.preventDefault();
      this.redo();
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
      const attributes = interiorComment(
        this.doc,
        inheritable(this.pendingFormat || (index > 0 ? this.doc.attributesAt(index - 1, 1) : {})),
        index,
        removed,
      );
      delta.insert(inserted, attributes);
    }
    this.pendingFormat = null;

    // Typed runs coalesce; a deletion starts a fresh step, so backspacing does
    // not get folded into the word it is removing.
    this.applyLocal(delta.chop(), {
      coalesce: removed === 0 && !inserted.includes('\n'),
      selection: { index, length: 0 },
    });

    // Plain characters land in the DOM correctly on their own. Anything that
    // changes structure or formatting needs a re-render to stay canonical.
    if (inserted.includes('\n') || removed > 0) {
      const caret = index + inserted.length;
      renderDelta(this.root, this.doc);
      this.setSelection(caret);
    }
    this.reportSelection();
  }

  /**
   * The `@word` the caret is sitting at the end of, if any.
   *
   * Returns `{ query, index }` where `index` is where the `@` is. Anchored to
   * the caret rather than found by scanning the document: an `@` somewhere else
   * in the message is a thing somebody wrote, not a thing they are typing.
   */
  mentionQuery() {
    const selection = this.getSelection();
    if (!selection || selection.length > 0) return null;
    const text = this.doc.toPlainText().slice(0, selection.index);

    const at = text.lastIndexOf('@');
    if (at === -1) return null;
    // Must start a word, or an email address offers to name people.
    if (at > 0 && !/[\s(\[]/.test(text[at - 1])) return null;

    const query = text.slice(at + 1);
    // A name has no spaces, and a long run without a match is somebody writing
    // prose that happens to contain an @.
    if (/[\s@]/.test(query) || query.length > 32) return null;
    return { query, index: at };
  }

  /**
   * Replace the `@query` at the caret with a mention of `user`.
   *
   * The inserted text is the display name; the id rides along as the attribute,
   * which is what keeps the mention meaning the same person after the words are
   * edited or the name is changed.
   */
  insertMention(user, at) {
    const total = this.doc.toPlainText().length;
    const index = Math.max(0, Math.min(at.index, total));
    const length = Math.min(at.query.length + 1, total - index);

    const delta = new Delta()
      .retain(index)
      .delete(length)
      .insert(`@${user.displayName}`, { [MENTION_ATTR]: user.id })
      // A trailing space, unmentioned, so the next word does not run into the
      // name and is not part of it.
      .insert(' ');

    this.pendingFormat = null;
    this.applyLocal(delta, { selection: { index, length } });
    renderDelta(this.root, this.doc);
    this.setSelection(index + user.displayName.length + 2);
    this.reportSelection();
  }

  /** Replace the current selection with `text`, as one op. */
  replaceSelection(text) {
    const selection = this.getSelection() || { index: this.doc.toPlainText().length, length: 0 };
    const attributes = interiorComment(
      this.doc,
      inheritable(
        this.pendingFormat ||
          (selection.index > 0 ? this.doc.attributesAt(selection.index - 1, 1) : {}),
      ),
      selection.index,
      selection.length,
    );

    const delta = new Delta()
      .retain(selection.index)
      .delete(selection.length)
      .insert(text, attributes);

    this.pendingFormat = null;
    this.applyLocal(delta, { selection });
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
    this.applyLocal(delta, { selection: { index, length } });
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
      const current = inheritable(this.doc.attributesAt(Math.max(0, selection.index - 1), 1));
      this.pendingFormat = { ...current, [name]: value };
      if (value === null) delete this.pendingFormat[name];
      return;
    }

    const delta = new Delta().retain(selection.index).retain(selection.length, { [name]: value });
    this.applyLocal(delta, { selection });
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

    // Every stored step was written against a document this change has just
    // moved. Rebase them, or undo starts deleting whatever happens to sit at
    // the offsets it remembers.
    rebaseStack(this.history.undo, delta);
    rebaseStack(this.history.redo, delta);

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
    // The history described a document that is being thrown away. Keeping it
    // would offer to undo edits against text that no longer exists, which is
    // worse than offering nothing.
    this.history.undo.length = 0;
    this.history.redo.length = 0;
    renderDelta(this.root, this.doc);
  }
}
