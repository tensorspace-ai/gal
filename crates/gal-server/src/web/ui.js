// Small DOM and formatting helpers shared by the application.

/**
 * Build an element.
 *
 * Everything user-supplied goes in through `textContent` or as a DOM node, so
 * there is no path where a display name or message body is parsed as HTML.
 */
export function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key === 'html') node.innerHTML = value; // only ever server-escaped snippets
    else if (key === 'dataset') Object.assign(node.dataset, value);
    else if (key === 'style') Object.assign(node.style, value);
    else if (key.startsWith('on') && typeof value === 'function') {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key in node && key !== 'list') {
      node[key] = value;
    } else {
      node.setAttribute(key, value);
    }
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined || child === false) continue;
    node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

export function clear(node) {
  node.textContent = '';
  return node;
}

/** Colour derived from the hue the server assigned to a user. */
export function userColor(user, lightness = 45) {
  const hue = user && typeof user.color === 'number' ? user.color : 210;
  return `hsl(${hue} 62% ${lightness}%)`;
}

export function initials(user) {
  const source = (user.displayName || user.name || '?').trim();
  const parts = source.split(/\s+/).filter(Boolean);
  if (parts.length === 0) return '?';
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
}

export function avatar(user, { size = 28, title = true } = {}) {
  return el('span', {
    class: 'avatar',
    text: initials(user),
    title: title ? `${user.displayName} (@${user.name})` : null,
    style: {
      width: `${size}px`,
      height: `${size}px`,
      background: userColor(user),
      fontSize: `${Math.max(9, Math.round(size * 0.4))}px`,
    },
  });
}

const MINUTE = 60 * 1000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** Compact relative time, in the style of an inbox. */
export function relativeTime(timestamp) {
  const delta = Date.now() - timestamp;
  if (delta < MINUTE) return 'now';
  if (delta < HOUR) return `${Math.floor(delta / MINUTE)}m`;
  if (delta < DAY) return `${Math.floor(delta / HOUR)}h`;
  if (delta < 7 * DAY) return `${Math.floor(delta / DAY)}d`;

  const date = new Date(timestamp);
  const sameYear = date.getFullYear() === new Date().getFullYear();
  return date.toLocaleDateString(undefined, {
    month: 'short',
    day: 'numeric',
    year: sameYear ? undefined : 'numeric',
  });
}

export function fullTime(timestamp) {
  return new Date(timestamp).toLocaleString(undefined, {
    dateStyle: 'medium',
    timeStyle: 'short',
  });
}

/** Transient message in the corner. */
export function toast(message, kind = 'info') {
  const host = document.getElementById('toasts');
  if (!host) return;
  const node = el('div', { class: `toast toast-${kind}`, text: message });
  host.appendChild(node);
  setTimeout(() => {
    node.classList.add('leaving');
    setTimeout(() => node.remove(), 250);
  }, kind === 'error' ? 6000 : 3200);
}

/**
 * Draw other people's carets over an editor.
 *
 * Positions come from a DOM Range, so they follow wrapping and font metrics
 * without the editor needing to know anything about layout.
 */
export class CursorLayer {
  constructor(container) {
    this.container = container;
    this.layer = el('div', { class: 'cursor-layer' });
    this.container.appendChild(this.layer);
    this.cursors = new Map();
  }

  set(userId, user, index, length, editorRoot) {
    this.cursors.set(userId, { user, index, length, editorRoot });
    this.render();
  }

  remove(userId) {
    if (this.cursors.delete(userId)) this.render();
  }

  clear() {
    this.cursors.clear();
    this.render();
  }

  render() {
    clear(this.layer);
    const base = this.container.getBoundingClientRect();

    for (const [, cursor] of this.cursors) {
      const rect = this.rectFor(cursor);
      if (!rect) continue;

      const caret = el('div', {
        class: 'remote-caret',
        style: {
          left: `${rect.left - base.left}px`,
          top: `${rect.top - base.top}px`,
          height: `${rect.height || 18}px`,
          background: userColor(cursor.user),
        },
      });
      const label = el('div', {
        class: 'remote-caret-label',
        text: cursor.user.displayName,
        style: { background: userColor(cursor.user) },
      });
      caret.appendChild(label);
      this.layer.appendChild(caret);
    }
  }

  rectFor(cursor) {
    const { editorRoot, index } = cursor;
    if (!editorRoot || !editorRoot.isConnected) return null;
    try {
      // Imported lazily to keep this module free of editor internals.
      const { indexToDom } = window.__galEditorHelpers || {};
      if (!indexToDom) return null;
      const position = indexToDom(editorRoot, index);
      const range = document.createRange();
      range.setStart(position.node, position.offset);
      range.collapse(true);
      const rects = range.getClientRects();
      if (rects.length > 0) return rects[0];
      // A collapsed range in an empty element has no rect; fall back to the
      // element's own box.
      const box = editorRoot.getBoundingClientRect();
      return { left: box.left, top: box.top, height: 18 };
    } catch {
      return null;
    }
  }
}

/** Ask for a value with a modal prompt. Resolves to null when cancelled. */
export function askFor(
  title,
  { placeholder = '', value = '', confirmLabel = 'OK', description = '', password = false } = {},
) {
  return new Promise((resolve) => {
    const input = el('input', {
      class: 'field',
      placeholder,
      value,
      autofocus: true,
      type: password ? 'password' : 'text',
    });
    let settled = false;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      overlay.remove();
      document.removeEventListener('keydown', onKey);
      resolve(result);
    };
    const onKey = (e) => {
      if (e.key === 'Escape') finish(null);
    };

    const dialog = el('div', { class: 'dialog' }, [
      el('h2', { text: title }),
      description ? el('p', { class: 'dialog-note', text: description }) : null,
      input,
      el('div', { class: 'dialog-actions' }, [
        el('button', { class: 'btn ghost', text: 'Cancel', onClick: () => finish(null) }),
        el('button', {
          class: 'btn primary',
          text: confirmLabel,
          onClick: () => finish(input.value.trim() || null),
        }),
      ]),
    ]);

    const overlay = el('div', { class: 'overlay', onClick: (e) => {
      if (e.target === overlay) finish(null);
    } }, [dialog]);

    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        finish(input.value.trim() || null);
      }
    });

    document.addEventListener('keydown', onKey);
    document.body.appendChild(overlay);
    input.focus();
  });
}

/** Yes/no confirmation. Resolves to a boolean. */
export function confirmAction(title, message, { confirmLabel = 'Confirm', danger = false } = {}) {
  return new Promise((resolve) => {
    const finish = (result) => {
      overlay.remove();
      resolve(result);
    };
    const dialog = el('div', { class: 'dialog' }, [
      el('h2', { text: title }),
      el('p', { class: 'dialog-note', text: message }),
      el('div', { class: 'dialog-actions' }, [
        el('button', { class: 'btn ghost', text: 'Cancel', onClick: () => finish(false) }),
        el('button', {
          class: `btn ${danger ? 'danger' : 'primary'}`,
          text: confirmLabel,
          onClick: () => finish(true),
        }),
      ]),
    ]);
    const overlay = el('div', { class: 'overlay', onClick: (e) => {
      if (e.target === overlay) finish(false);
    } }, [dialog]);
    document.body.appendChild(overlay);
  });
}
