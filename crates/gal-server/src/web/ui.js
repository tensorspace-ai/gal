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

/**
 * An inline SVG icon.
 *
 * Built as DOM rather than as markup, so icons need no exception to the rule
 * that nothing in this client is assembled by string concatenation. They also
 * inherit `currentColor`, which is what lets one icon sit in a toolbar button
 * and light up with it.
 */
export function icon(path, { size = 15 } = {}) {
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  for (const [name, value] of Object.entries({
    viewBox: '0 0 24 24',
    width: String(size),
    height: String(size),
    fill: 'none',
    stroke: 'currentColor',
    'stroke-width': '1.9',
    'stroke-linecap': 'round',
    'stroke-linejoin': 'round',
    'aria-hidden': 'true',
  })) {
    svg.setAttribute(name, value);
  }
  const shape = document.createElementNS(NS, 'path');
  shape.setAttribute('d', path);
  svg.appendChild(shape);
  return svg;
}

export const ICONS = {
  comment: 'M21 12a8 8 0 0 1-8 8H8l-4 3v-4.6A8 8 0 0 1 13 4a8 8 0 0 1 8 8Z',
  link:'M10.6 13.4a4 4 0 0 0 5.7 0l3.1-3.1a4 4 0 0 0-5.7-5.7l-1.7 1.8M13.4 10.6a4 4 0 0 0-5.7 0l-3.1 3.1a4 4 0 1 0 5.7 5.7l1.7-1.8',
  paperclip:
    'M20 10.5 11.6 19a4.6 4.6 0 0 1-6.5-6.5l8.5-8.4a3 3 0 1 1 4.3 4.3l-8.5 8.4a1.5 1.5 0 1 1-2.1-2.1l7.6-7.6',
};

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

/** A file size a person would read out loud. */
export function fileSize(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return '';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['kB', 'MB', 'GB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
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

/** Wall-clock time, as a chat shows beside a message. */
export function clockTime(timestamp) {
  return new Date(timestamp).toLocaleTimeString(undefined, {
    hour: 'numeric',
    minute: '2-digit',
  });
}

/**
 * The same time without the meridiem, for the narrow gutter beside a grouped
 * message.
 *
 * Dropping the part rather than forcing a 24-hour clock keeps whatever hour
 * cycle the reader's locale uses; the full timestamp is still on the title.
 */
export function shortClockTime(timestamp) {
  return new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' })
    .formatToParts(new Date(timestamp))
    .filter((part) => part.type !== 'dayPeriod')
    .map((part) => part.value)
    .join('')
    .trim();
}

export function sameDay(a, b) {
  const x = new Date(a);
  const y = new Date(b);
  return (
    x.getFullYear() === y.getFullYear() &&
    x.getMonth() === y.getMonth() &&
    x.getDate() === y.getDate()
  );
}

/** Heading for a day's worth of messages. */
export function dayLabel(timestamp) {
  const now = Date.now();
  if (sameDay(timestamp, now)) return 'Today';
  if (sameDay(timestamp, now - DAY)) return 'Yesterday';
  const date = new Date(timestamp);
  return date.toLocaleDateString(undefined, {
    weekday: 'long',
    month: 'long',
    day: 'numeric',
    year: date.getFullYear() === new Date().getFullYear() ? undefined : 'numeric',
  });
}

/**
 * Message in the corner.
 *
 * Toasts are this client's entire error surface, which puts two requirements on
 * them that they did not meet. They carry `role`, so a screen reader hears
 * them — every failure in the product was previously silent, since a bare `div`
 * appearing in the corner is not an event any assistive technology reports. And
 * an *error* stays until it is dismissed: it used to fade after six seconds
 * from a container that was `pointer-events: none`, so it could not be read
 * twice, copied, or acted on, and an error you happened to miss was an error
 * that never happened.
 */
export function toast(message, kind = 'info') {
  const host = document.getElementById('toasts');
  if (!host) return;

  const node = el('div', {
    class: `toast toast-${kind}`,
    // `alert` is assertive and interrupts; right for a failure, too much for
    // "Reconnecting this wave…".
    role: kind === 'error' ? 'alert' : 'status',
  }, [el('span', { class: 'toast-text', text: message })]);

  const dismiss = () => {
    node.classList.add('leaving');
    setTimeout(() => node.remove(), 250);
  };

  if (kind === 'error') {
    node.appendChild(el('button', {
      class: 'toast-close',
      text: '×',
      'aria-label': 'Dismiss',
      onClick: dismiss,
    }));
  } else {
    setTimeout(dismiss, 3200);
  }

  host.appendChild(node);
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
