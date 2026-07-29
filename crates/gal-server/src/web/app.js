// Gal — application shell, inbox and wave view.

import { Delta, compose } from './ot.js';
import { BlipDoc, Connection } from './client.js';
import { Editor, indexToDom, renderDelta } from './editor.js';
import {
  askFor,
  avatar,
  clear,
  confirmAction,
  CursorLayer,
  el,
  fullTime,
  relativeTime,
  toast,
  userColor,
} from './ui.js';

// The cursor layer positions carets via the editor's index mapping.
window.__galEditorHelpers = { indexToDom };

const state = {
  me: null,
  users: [],
  inbox: new Map(),
  waveId: null,
  wave: null,
  docs: new Map(), // blipId -> BlipDoc
  editors: new Map(), // blipId -> Editor
  blipNodes: new Map(), // blipId -> element
  presence: [],
  remoteCursors: new Map(),
  cursorLayer: null,
  activeEditor: null,
  playback: null,
  search: null,
  filter: 'all',
};

const conn = new Connection();

// --- authentication -----------------------------------------------------

async function api(path, options = {}) {
  const response = await fetch(path, {
    headers: { 'Content-Type': 'application/json' },
    ...options,
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || 'Request failed.');
  return body;
}

function renderAuth(serverInfo) {
  const app = clear(document.getElementById('app'));
  let mode = serverInfo.openRegistration ? 'register' : 'login';

  const draw = () => {
    clear(app);
    const isRegister = mode === 'register';
    const name = el('input', { class: 'field', placeholder: 'username', autocomplete: 'username' });
    const display = el('input', { class: 'field', placeholder: 'display name (optional)' });
    const password = el('input', {
      class: 'field',
      type: 'password',
      placeholder: 'password',
      autocomplete: isRegister ? 'new-password' : 'current-password',
    });
    const error = el('p', { class: 'auth-error' });

    const submit = async () => {
      error.textContent = '';
      try {
        const body = isRegister
          ? { name: name.value, displayName: display.value, password: password.value }
          : { name: name.value, password: password.value };
        const result = await api(isRegister ? '/api/register' : '/api/login', {
          method: 'POST',
          body: JSON.stringify(body),
        });
        state.me = result.user;
        start();
      } catch (e) {
        error.textContent = e.message;
      }
    };

    const form = el('form', {
      class: 'auth-form',
      onSubmit: (e) => {
        e.preventDefault();
        submit();
      },
    }, [
      el('h1', { class: 'auth-logo', text: 'Gal' }),
      el('p', {
        class: 'auth-tagline',
        text: 'Conversations you write together, not just messages you send.',
      }),
      name,
      isRegister ? display : null,
      password,
      el('button', { class: 'btn primary wide', type: 'submit', text: isRegister ? 'Create account' : 'Sign in' }),
      error,
      serverInfo.openRegistration
        ? el('button', {
            class: 'btn link',
            type: 'button',
            text: isRegister ? 'I already have an account' : 'Create an account',
            onClick: () => {
              mode = isRegister ? 'login' : 'register';
              draw();
            },
          })
        : null,
    ]);

    app.appendChild(el('div', { class: 'auth-screen' }, [form]));
    name.focus();
  };

  draw();
}

// --- application shell --------------------------------------------------

function renderShell() {
  const app = clear(document.getElementById('app'));

  const searchInput = el('input', {
    class: 'search-input',
    placeholder: 'Search waves',
    type: 'search',
    onInput: (e) => {
      const query = e.target.value.trim();
      if (!query) {
        state.search = null;
        renderInbox();
        return;
      }
      conn.send({ type: 'search', query });
    },
  });

  const sidebar = el('aside', { class: 'sidebar' }, [
    el('div', { class: 'sidebar-head' }, [
      el('div', { class: 'brand' }, [
        el('span', { class: 'brand-mark' }),
        el('span', { class: 'brand-name', text: 'Gal' }),
      ]),
      el('button', {
        class: 'btn primary new-wave',
        text: 'New wave',
        title: 'Start a new wave (n)',
        onClick: startNewWave,
      }),
    ]),
    el('div', { class: 'search-row' }, [searchInput]),
    el('div', { class: 'filters', id: 'filters' }),
    el('div', { class: 'inbox', id: 'inbox' }),
    el('div', { class: 'sidebar-foot' }, [
      el('div', { class: 'me' }, [
        avatar(state.me, { size: 26 }),
        el('span', { class: 'me-name', text: state.me.displayName }),
      ]),
      el('span', { class: 'status-dot', id: 'status', title: 'Connection status' }),
      el('button', {
        class: 'btn link',
        text: 'Password',
        title: 'Change your password',
        onClick: changePassword,
      }),
      el('button', {
        class: 'btn link',
        text: 'Sign out',
        onClick: async () => {
          await api('/api/logout', { method: 'POST' });
          location.reload();
        },
      }),
    ]),
  ]);

  app.appendChild(el('div', { class: 'layout' }, [
    sidebar,
    el('main', { class: 'wave-pane', id: 'wave-pane' }),
  ]));

  renderFilters();
  renderInbox();
  renderEmptyWave();
  showInbox(!state.waveId);
  updateStatus(conn.status);
}

function renderFilters() {
  const host = clear(document.getElementById('filters'));
  const options = [
    ['all', 'All'],
    ['unread', 'Unread'],
    ['archived', 'Archived'],
  ];
  for (const [key, label] of options) {
    host.appendChild(el('button', {
      class: `filter ${state.filter === key ? 'active' : ''}`,
      text: label,
      onClick: () => {
        state.filter = key;
        renderFilters();
        renderInbox();
      },
    }));
  }
}

function inboxRows() {
  const rows = [...state.inbox.values()];
  const filtered = rows.filter((row) => {
    if (state.filter === 'unread') return row.unreadCount > 0 && !row.flags.archived;
    if (state.filter === 'archived') return row.flags.archived;
    return !row.flags.archived;
  });
  return filtered.sort((a, b) => b.lastModified - a.lastModified);
}

function renderInbox() {
  const host = clear(document.getElementById('inbox'));

  if (state.search) {
    host.appendChild(el('div', { class: 'inbox-label', text: `Results for “${state.search.query}”` }));
    if (state.search.hits.length === 0) {
      host.appendChild(el('div', { class: 'empty-note', text: 'Nothing matched.' }));
    }
    for (const hit of state.search.hits) {
      host.appendChild(el('button', {
        class: 'inbox-row',
        onClick: () => openWave(hit.waveId),
      }, [
        el('div', { class: 'inbox-title', text: hit.title }),
        // The snippet arrives pre-escaped from SQLite with <mark> highlights.
        el('div', { class: 'inbox-snippet', html: hit.snippet }),
        el('div', { class: 'inbox-time', text: relativeTime(hit.timestamp) }),
      ]));
    }
    return;
  }

  const rows = inboxRows();
  if (rows.length === 0) {
    host.appendChild(el('div', { class: 'empty-note', text:
      state.filter === 'all' ? 'No waves yet. Start one.' : 'Nothing here.' }));
    return;
  }

  for (const row of rows) {
    const others = row.participants.filter((p) => p.id !== state.me.id);
    const faces = el('div', { class: 'faces' },
      (others.length ? others : row.participants).slice(0, 3).map((p) => avatar(p, { size: 22 })));

    host.appendChild(el('button', {
      class: `inbox-row ${row.id === state.waveId ? 'active' : ''} ${row.unreadCount ? 'unread' : ''}`,
      onClick: () => openWave(row.id),
    }, [
      el('div', { class: 'inbox-row-top' }, [
        faces,
        el('div', { class: 'inbox-title', text: row.title }),
        el('div', { class: 'inbox-time', text: relativeTime(row.lastModified) }),
      ]),
      el('div', { class: 'inbox-snippet', text: row.snippet || 'No messages yet' }),
      row.unreadCount
        ? el('span', { class: 'badge', text: String(row.unreadCount) })
        : null,
    ]));
  }
}

/**
 * On a narrow screen the sidebar and the wave pane share the viewport, so only
 * one is shown at a time. Reading a wave switches to it; the back button and an
 * empty selection switch back.
 */
function showInbox(show) {
  const layout = document.querySelector('.layout');
  if (layout) layout.classList.toggle('show-inbox', show);
}

function updateStatus(status) {
  const dot = document.getElementById('status');
  if (!dot) return;
  dot.className = `status-dot ${status}`;
  dot.title = status === 'online' ? 'Connected' : 'Reconnecting…';
}

// --- wave view ----------------------------------------------------------

function renderEmptyWave() {
  const pane = clear(document.getElementById('wave-pane'));
  pane.appendChild(el('div', { class: 'empty-wave' }, [
    el('h2', { text: 'Pick a wave' }),
    el('p', { text: 'Or start a new one. Everyone in a wave can edit every message in it, live.' }),
  ]));
}

function openWave(waveId) {
  if (state.waveId === waveId) return;
  if (state.waveId) conn.closeWave(state.waveId);
  teardownWave();
  state.waveId = waveId;
  conn.openWave(waveId);
  showInbox(false);
  renderInbox();
}

function teardownWave() {
  for (const editor of state.editors.values()) editor.destroy();
  state.editors.clear();
  state.docs.clear();
  state.blipNodes.clear();
  state.remoteCursors.clear();
  state.presence = [];
  state.playback = null;
  state.wave = null;
  state.activeEditor = null;
  state.cursorLayer = null;
}

function rootWavelet() {
  return state.wave && state.wave.wavelets.find((w) => w.kind === 'conversation');
}

function allBlips() {
  if (!state.wave) return [];
  return state.wave.wavelets.flatMap((w) => w.blips.map((b) => ({ ...b, wavelet: w })));
}

function renderWave() {
  if (!state.wave) return;
  const pane = clear(document.getElementById('wave-pane'));
  const root = rootWavelet();
  const title = root ? root.title : 'Wave';

  state.blipNodes.clear();

  const header = el('header', { class: 'wave-head' }, [
    el('button', {
      class: 'btn ghost back-to-inbox',
      text: '‹',
      title: 'Back to the inbox',
      onClick: () => showInbox(true),
    }),
    el('div', { class: 'wave-head-main' }, [
      el('h1', {
        class: 'wave-title',
        text: title,
        title: 'Click to rename',
        onClick: async () => {
          const next = await askFor('Rename wave', { value: title, confirmLabel: 'Rename' });
          if (next && root) conn.send({ type: 'setTitle', waveletId: root.id, title: next });
        },
      }),
      el('div', { class: 'wave-sub' }, [
        el('span', { class: 'presence', id: 'presence' }),
        el('span', { class: 'participants', id: 'participants' }),
      ]),
    ]),
    el('div', { class: 'wave-actions' }, [
      formatToolbar(),
      el('button', {
        class: 'btn ghost',
        text: 'Playback',
        title: 'Replay how this wave was written',
        onClick: () => conn.send({ type: 'requestPlayback', waveId: state.waveId }),
      }),
      el('button', {
        class: 'btn ghost',
        text: state.wave.flags.archived ? 'Unarchive' : 'Archive',
        onClick: () => conn.send({
          type: 'setFlags',
          waveId: state.waveId,
          flags: { ...state.wave.flags, archived: !state.wave.flags.archived },
        }),
      }),
    ]),
  ]);

  const thread = el('div', { class: 'thread', id: 'thread' });
  const scroller = el('div', { class: 'thread-scroll' }, [
    thread,
    el('div', { class: 'thread-foot' }, [
      el('button', {
        class: 'btn primary',
        text: 'Add message',
        onClick: () => {
          if (root) conn.send({ type: 'createBlip', waveletId: root.id });
        },
      }),
    ]),
  ]);

  pane.appendChild(header);
  pane.appendChild(scroller);

  state.cursorLayer = new CursorLayer(scroller);
  renderParticipants();
  renderPresence();
  renderThread();
}

function renderParticipants() {
  const host = document.getElementById('participants');
  if (!host) return;
  clear(host);
  const root = rootWavelet();
  if (!root) return;

  for (const participant of root.participants) {
    const face = avatar(participant, { size: 24 });
    face.classList.add('clickable');
    face.addEventListener('click', async () => {
      if (participant.id === state.me.id) return;
      const ok = await confirmAction(
        'Remove participant',
        `Remove ${participant.displayName} from this wave?`,
        { confirmLabel: 'Remove', danger: true },
      );
      if (ok) {
        conn.send({ type: 'removeParticipant', waveletId: root.id, userId: participant.id });
      }
    });
    host.appendChild(face);
  }

  host.appendChild(el('button', {
    class: 'add-participant',
    text: '+',
    title: 'Add someone to this wave',
    onClick: async () => {
      // Only people you already share a wave with are suggested; anyone else is
      // added by typing their exact username. The server no longer hands out a
      // full member directory.
      const known = state.users
        .filter((u) => !root.participants.some((p) => p.id === u.id))
        .map((u) => u.name);
      const name = await askFor('Add participant', {
        placeholder: 'username',
        description: known.length ? `People you know here: ${known.slice(0, 8).join(', ')}` : '',
        confirmLabel: 'Add',
      });
      if (name) conn.send({ type: 'addParticipant', waveletId: root.id, name });
    },
  }));
}

function renderPresence() {
  const host = document.getElementById('presence');
  if (!host) return;
  clear(host);
  const others = state.presence.filter((p) => p.user.id !== state.me.id);
  if (others.length === 0) return;

  host.appendChild(el('span', { class: 'presence-label', text: 'here now' }));
  for (const entry of others) {
    host.appendChild(el('span', {
      class: 'presence-dot',
      title: entry.user.displayName,
      style: { background: userColor(entry.user) },
    }));
  }
}

/** Build the blip tree, reusing existing nodes so editors keep their state. */
function renderThread() {
  const host = document.getElementById('thread');
  if (!host) return;
  clear(host);

  const blips = allBlips();
  const byParent = new Map();
  for (const blip of blips) {
    const key = blip.parent || `root:${blip.waveletId}`;
    if (!byParent.has(key)) byParent.set(key, []);
    byParent.get(key).push(blip);
  }
  // Same total order the server uses. Sorting on seq alone leaves ties to
  // arrival order, which differs between clients.
  for (const list of byParent.values()) {
    list.sort((a, b) => a.seq - b.seq || a.createdAt - b.createdAt || (a.id < b.id ? -1 : 1));
  }

  const privateReplies = new Map();
  for (const wavelet of state.wave.wavelets) {
    if (wavelet.kind === 'privateReply' && wavelet.anchorBlip) {
      if (!privateReplies.has(wavelet.anchorBlip)) privateReplies.set(wavelet.anchorBlip, []);
      privateReplies.get(wavelet.anchorBlip).push(wavelet);
    }
  }

  const renderInto = (container, key, depth) => {
    for (const blip of byParent.get(key) || []) {
      const node = blipElement(blip, depth);
      container.appendChild(node);

      const children = el('div', { class: 'blip-children' });
      node.appendChild(children);
      renderInto(children, blip.id, depth + 1);

      // A private reply hangs off the blip it was branched from.
      for (const wavelet of privateReplies.get(blip.id) || []) {
        const aside = el('div', { class: 'private-thread' }, [
          el('div', { class: 'private-label' }, [
            el('span', { text: 'Private reply' }),
            ...wavelet.participants.map((p) => avatar(p, { size: 18 })),
          ]),
        ]);
        children.appendChild(aside);
        renderInto(aside, `root:${wavelet.id}`, depth + 1);
      }
    }
  };

  const root = rootWavelet();
  if (root) renderInto(host, `root:${root.id}`, 0);
}

function blipElement(blip, depth) {
  const author = state.users.find((u) => u.id === blip.author) || {
    id: blip.author,
    name: '?',
    displayName: 'Unknown',
    color: 0,
  };

  const doc = state.docs.get(blip.id) || new BlipDoc(blip.id, blip.content, blip.revision);
  state.docs.set(blip.id, doc);

  const body = el('div', { class: 'blip-body' });
  const article = el('article', {
    class: `blip ${blip.unread ? 'unread' : ''}`,
    dataset: { blipId: blip.id, depth: String(depth) },
  });

  const contributors = (blip.contributors || []).filter((id) => id !== blip.author);
  const head = el('div', { class: 'blip-head' }, [
    avatar(author, { size: 26 }),
    el('span', { class: 'blip-author', text: author.displayName }),
    el('time', {
      class: 'blip-time',
      text: relativeTime(blip.lastModified),
      title: fullTime(blip.lastModified),
    }),
    contributors.length
      ? el('span', {
          class: 'blip-contributors',
          title: 'Also edited by others',
          text: `+${contributors.length}`,
        })
      : null,
    el('div', { class: 'blip-actions' }, [
      actionButton('Reply', 'Reply below this message', () => {
        conn.send({ type: 'createBlip', waveletId: blip.waveletId, parent: blip.id });
      }),
      actionButton('Privately', 'Start a private side conversation', async () => {
        const name = await askFor('Private reply', {
          placeholder: 'username',
          description: 'Only you and the people you name will see this thread.',
          confirmLabel: 'Start',
        });
        if (name) {
          conn.send({
            type: 'privateReply',
            waveletId: blip.waveletId,
            anchor: blip.id,
            participants: [name],
          });
        }
      }),
      blip.author === state.me.id
        ? actionButton('Delete', 'Delete this message', async () => {
            const ok = await confirmAction('Delete message', 'This cannot be undone.', {
              confirmLabel: 'Delete',
              danger: true,
            });
            if (ok) conn.send({ type: 'deleteBlip', blipId: blip.id });
          })
        : null,
    ]),
  ]);

  article.appendChild(head);
  article.appendChild(body);
  article.style.setProperty('--author-color', userColor(author));

  const editorRoot = el('div', { class: 'editor' });
  body.appendChild(editorRoot);

  if (state.playback) {
    editorRoot.contentEditable = 'false';
    renderDelta(editorRoot, doc.doc);
  } else {
    const editor = new Editor(editorRoot, doc.doc, {
      onChange: (delta) => onLocalEdit(blip.id, delta),
      onSelectionChange: (selection) => {
        state.activeEditor = editor;
        updateToolbar();
        conn.send({
          type: 'cursor',
          waveId: state.waveId,
          blipId: blip.id,
          index: selection.index,
          length: selection.length,
        });
      },
    });
    // Keep the model and the editor pointing at the same document object.
    editor.doc = doc.doc;
    state.editors.set(blip.id, editor);
  }

  state.blipNodes.set(blip.id, article);
  return article;
}

function actionButton(label, title, handler) {
  return el('button', { class: 'blip-action', text: label, title, onClick: handler });
}

/**
 * Re-apply edits that were never acknowledged, on top of a fresh snapshot.
 *
 * This is what stops typing from vanishing when the connection drops mid-word.
 * The op was written against an older revision, so it is only replayed when it
 * still addresses a document of at least that length; if other people rewrote
 * the message while we were away it is dropped, and we say so rather than
 * splicing text into the wrong place.
 */
function replayUnsentEdits(unsent) {
  let dropped = 0;

  for (const [blipId, work] of unsent) {
    const doc = state.docs.get(blipId);
    if (!doc) {
      dropped += 1;
      continue;
    }
    if (work.baseLength() > doc.doc.length) {
      dropped += 1;
      continue;
    }

    const toSend = doc.applyLocal(work);
    const editor = state.editors.get(blipId);
    if (editor) {
      editor.doc = doc.doc;
      editor.reset(doc.doc);
    }
    if (toSend) {
      // Same op id as before the reconnect, so if the server already applied
      // this work it acknowledges it rather than applying it a second time.
      conn.send({
        type: 'submit',
        blipId,
        revision: doc.revision,
        delta: toSend,
        opId: doc.outstandingOpId,
      });
    }
  }

  if (dropped > 0) {
    toast(
      `${dropped} unsent change${dropped === 1 ? '' : 's'} could not be restored.`,
      'error',
    );
  }
}

function onLocalEdit(blipId, delta) {
  const doc = state.docs.get(blipId);
  if (!doc) return;

  const toSend = doc.applyLocal(delta);
  const editor = state.editors.get(blipId);
  // Both applied the same op to the same base; share one object so they can
  // never drift apart.
  if (editor) editor.doc = doc.doc;

  if (toSend) {
    conn.send({
      type: 'submit',
      blipId,
      revision: doc.revision,
      delta: toSend,
      opId: doc.outstandingOpId,
    });
  }
}

// --- formatting toolbar -------------------------------------------------

const FORMATS = [
  ['bold', 'B', 'Bold  ⌘B'],
  ['italic', 'I', 'Italic  ⌘I'],
  ['underline', 'U', 'Underline  ⌘U'],
  ['strike', 'S', 'Strikethrough'],
  ['code', '‹›', 'Code'],
];

function formatToolbar() {
  const bar = el('div', { class: 'toolbar', id: 'toolbar' });
  for (const [name, label, title] of FORMATS) {
    bar.appendChild(el('button', {
      class: `tool tool-${name}`,
      text: label,
      title,
      // Keep focus in the editor so the selection survives the click.
      onMousedown: (e) => e.preventDefault(),
      onClick: () => applyFormat(name),
    }));
  }
  bar.appendChild(el('button', {
    class: 'tool',
    text: '🔗',
    title: 'Add a link',
    onMousedown: (e) => e.preventDefault(),
    onClick: async () => {
      if (!state.activeEditor) return;
      const url = await askFor('Link', { placeholder: 'https://…', confirmLabel: 'Link' });
      if (url) state.activeEditor.format('link', url);
      updateToolbar();
    },
  }));
  return bar;
}

function applyFormat(name) {
  const editor = state.activeEditor;
  if (!editor) return;
  editor.format(name, editor.isActive(name) ? null : true);
  updateToolbar();
}

function updateToolbar() {
  const bar = document.getElementById('toolbar');
  if (!bar || !state.activeEditor) return;
  for (const [name] of FORMATS) {
    const button = bar.querySelector(`.tool-${name}`);
    if (button) button.classList.toggle('active', state.activeEditor.isActive(name));
  }
}

// --- playback -----------------------------------------------------------

function renderPlayback(frames) {
  state.playback = { frames, position: frames.length };
  renderWave();

  const pane = document.getElementById('wave-pane');
  const bar = el('div', { class: 'playback-bar' }, [
    el('button', { class: 'btn ghost', text: 'Exit playback', onClick: () => {
      state.playback = null;
      renderWave();
    } }),
    el('input', {
      class: 'playback-slider',
      type: 'range',
      min: '0',
      max: String(frames.length),
      value: String(frames.length),
      onInput: (e) => setPlaybackPosition(Number(e.target.value)),
    }),
    el('span', { class: 'playback-label', id: 'playback-label' }),
  ]);
  pane.appendChild(bar);
  setPlaybackPosition(frames.length);
}

function setPlaybackPosition(position) {
  if (!state.playback) return;
  state.playback.position = position;
  const { frames } = state.playback;

  // Replay the op log up to this point to rebuild every document.
  const docs = new Map();
  for (let i = 0; i < position; i += 1) {
    const frame = frames[i];
    const current = docs.get(frame.blipId) || new Delta();
    docs.set(frame.blipId, compose(current, new Delta(frame.delta)));
  }

  for (const [blipId, node] of state.blipNodes) {
    const editorRoot = node.querySelector('.editor');
    const content = docs.get(blipId);
    node.classList.toggle('not-yet', !content);
    if (editorRoot) renderDelta(editorRoot, content || new Delta());
  }

  const label = document.getElementById('playback-label');
  if (label) {
    const frame = frames[position - 1];
    label.textContent = frame
      ? `${position} / ${frames.length} · ${fullTime(frame.timestamp)}`
      : 'Before the beginning';
  }
}

async function changePassword() {
  const current = await askFor('Change password', {
    placeholder: 'current password',
    description: 'Changing your password signs you out everywhere else.',
    confirmLabel: 'Next',
    password: true,
  });
  if (!current) return;
  const next = await askFor('Change password', {
    placeholder: 'new password (at least 8 characters)',
    confirmLabel: 'Change',
    password: true,
  });
  if (!next) return;

  try {
    await api('/api/password', {
      method: 'POST',
      body: JSON.stringify({ currentPassword: current, newPassword: next }),
    });
    toast('Password changed. Other sessions were signed out.');
  } catch (e) {
    toast(e.message, 'error');
  }
}

// --- new wave -----------------------------------------------------------

async function startNewWave() {
  const title = await askFor('New wave', {
    placeholder: 'What is this about?',
    confirmLabel: 'Create',
    description: 'You can add people once it exists.',
  });
  if (title) conn.send({ type: 'createWave', title, participants: [] });
}

// --- server messages ----------------------------------------------------

conn.on('status', updateStatus);

conn.on('welcome', (message) => {
  state.me = message.user;
  state.inbox = new Map(message.inbox.map((row) => [row.id, row]));
  renderShell();
  // Restore the wave named in the URL, so a link to a wave works.
  const fromUrl = location.pathname.startsWith('/wave/') ? location.pathname.slice(6) : null;
  if (fromUrl) openWave(fromUrl);
});

conn.on('waveState', (message) => {
  const wave = message.wave;

  // Reopening a wave we already had open — a reconnect, or a resync after a
  // rejected op. Anything typed since the last acknowledgement exists only in
  // this tab, so rescue it before the snapshot replaces our documents.
  const unsent = new Map();
  if (state.waveId === wave.id) {
    for (const [blipId, doc] of state.docs) {
      const work = doc.pendingWork();
      if (work && !work.isEmpty()) unsent.set(blipId, work);
    }
    for (const editor of state.editors.values()) editor.destroy();
    state.editors.clear();
    state.docs.clear();
  }
  state.waveId = wave.id;
  state.wave = wave;
  state.playback = null;
  // Covers waves the server opened for us (a freshly created one), which the
  // connection would otherwise not know to re-open after a reconnect.
  conn.track(wave.id);
  history.replaceState(null, '', `/wave/${wave.id}`);

  renderWave();
  replayUnsentEdits(unsent);
  renderInbox();
  // Covers arriving here without going through openWave — creating a wave, or
  // reconnecting into one that was already open.
  showInbox(false);
  conn.send({ type: 'markRead', waveId: wave.id });

  // Put the caret in the first empty message, which is where a new wave lands.
  const empty = allBlips().find((b) => (b.content.ops || []).length === 0);
  if (empty) {
    const editor = state.editors.get(empty.id);
    if (editor) editor.root.focus();
  }
});

conn.on('op', (message) => {
  const doc = state.docs.get(message.blipId);
  if (!doc) return;
  const applied = doc.applyRemote(new Delta(message.delta), message.revision);
  const editor = state.editors.get(message.blipId);
  if (editor) {
    editor.applyRemote(applied, doc.doc);
  } else {
    const node = state.blipNodes.get(message.blipId);
    if (node) renderDelta(node.querySelector('.editor'), doc.doc);
  }
  if (state.cursorLayer) state.cursorLayer.render();
});

conn.on('ack', (message) => {
  const doc = state.docs.get(message.blipId);
  if (!doc) return;
  const next = doc.acknowledge(message.revision);
  if (next) {
    conn.send({
      type: 'submit',
      blipId: message.blipId,
      revision: doc.revision,
      delta: next,
      opId: doc.outstandingOpId,
    });
  }
});

conn.on('blipAdded', (message) => {
  if (message.waveId !== state.waveId || !state.wave) return;
  const wavelet = state.wave.wavelets.find((w) => w.id === message.blip.waveletId);
  if (!wavelet || wavelet.blips.some((b) => b.id === message.blip.id)) return;

  wavelet.blips.push(message.blip);
  renderThread();

  // Focus a message we just created ourselves.
  if (message.blip.author === state.me.id) {
    const editor = state.editors.get(message.blip.id);
    if (editor) {
      editor.root.focus();
      editor.root.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
  }
});

conn.on('blipRemoved', (message) => {
  if (message.waveId !== state.waveId || !state.wave) return;
  for (const wavelet of state.wave.wavelets) {
    wavelet.blips = wavelet.blips.filter((b) => b.id !== message.blipId);
  }
  const editor = state.editors.get(message.blipId);
  if (editor) editor.destroy();
  state.editors.delete(message.blipId);
  state.docs.delete(message.blipId);
  renderThread();
});

conn.on('titleChanged', (message) => {
  if (state.wave && message.waveId === state.waveId) {
    const wavelet = state.wave.wavelets.find((w) => w.id === message.waveletId);
    if (wavelet) wavelet.title = message.title;
    const heading = document.querySelector('.wave-title');
    if (heading) heading.textContent = message.title;
  }
});

conn.on('participantAdded', (message) => {
  if (!state.users.some((u) => u.id === message.user.id)) state.users.push(message.user);
  if (state.wave && message.waveId === state.waveId) {
    const wavelet = state.wave.wavelets.find((w) => w.id === message.waveletId);
    if (wavelet && !wavelet.participants.some((p) => p.id === message.user.id)) {
      wavelet.participants.push(message.user);
    }
    renderParticipants();
  }
});

conn.on('participantRemoved', (message) => {
  if (state.wave && message.waveId === state.waveId) {
    const wavelet = state.wave.wavelets.find((w) => w.id === message.waveletId);
    if (wavelet) wavelet.participants = wavelet.participants.filter((p) => p.id !== message.userId);
    renderParticipants();
  }
});

conn.on('waveletAdded', (message) => {
  if (state.wave && message.waveId === state.waveId) {
    if (!state.wave.wavelets.some((w) => w.id === message.wavelet.id)) {
      state.wave.wavelets.push(message.wavelet);
    }
    renderThread();
  }
});

conn.on('inboxUpdated', (message) => {
  state.inbox.set(message.summary.id, message.summary);
  renderInbox();
});

conn.on('waveRemoved', (message) => {
  state.inbox.delete(message.waveId);
  if (state.waveId === message.waveId) {
    conn.openWaves.delete(message.waveId);
    state.waveId = null;
    teardownWave();
    renderEmptyWave();
    showInbox(true);
    history.replaceState(null, '', '/');
    toast('You were removed from that wave.');
  }
  renderInbox();
});

conn.on('presence', (message) => {
  if (message.waveId !== state.waveId) return;
  state.presence = message.users;
  renderPresence();

  // Drop carets belonging to people who left.
  if (state.cursorLayer) {
    const here = new Set(message.users.map((u) => u.user.id));
    for (const userId of [...state.remoteCursors.keys()]) {
      if (!here.has(userId)) {
        state.remoteCursors.delete(userId);
        state.cursorLayer.remove(userId);
      }
    }
  }
});

conn.on('cursor', (message) => {
  if (message.waveId !== state.waveId || !state.cursorLayer) return;
  const editor = state.editors.get(message.blipId);
  const user = state.presence.find((p) => p.user.id === message.userId);
  if (!editor || !user) return;
  state.remoteCursors.set(message.userId, message);
  state.cursorLayer.set(message.userId, user.user, message.index, message.length, editor.root);
});

conn.on('playback', (message) => {
  if (message.waveId !== state.waveId) return;
  if (message.frames.length === 0) {
    toast('Nothing to play back yet.');
    return;
  }
  renderPlayback(message.frames);
});

conn.on('searchResults', (message) => {
  state.search = message;
  renderInbox();
});

conn.on('error', (message) => {
  if (message.code === 'resync' && message.blipId) {
    // Our op could not be transformed; reopen the wave for a clean snapshot.
    toast('Reconnecting this wave…');
    if (state.waveId) conn.openWave(state.waveId);
    return;
  }
  toast(message.message, 'error');
});

// --- keyboard shortcuts -------------------------------------------------

document.addEventListener('keydown', (event) => {
  const meta = event.metaKey || event.ctrlKey;
  if (meta && !event.shiftKey) {
    const shortcuts = { b: 'bold', i: 'italic', u: 'underline' };
    const format = shortcuts[event.key.toLowerCase()];
    if (format && state.activeEditor) {
      event.preventDefault();
      applyFormat(format);
      return;
    }
  }
  // `n` for a new wave, but not while typing.
  const typing = document.activeElement && (
    document.activeElement.isContentEditable ||
    ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName)
  );
  if (event.key === 'n' && !typing && !meta) {
    event.preventDefault();
    startNewWave();
  }
});

window.addEventListener('resize', () => {
  if (state.cursorLayer) state.cursorLayer.render();
});

// --- boot ---------------------------------------------------------------

async function start() {
  try {
    state.users = await api('/api/users');
  } catch {
    state.users = [];
  }
  conn.connect();
}

async function boot() {
  const info = await api('/api/server').catch(() => ({ openRegistration: true }));
  try {
    const session = await api('/api/me');
    state.me = session.user;
    start();
  } catch {
    renderAuth(info);
  }
}

boot();
