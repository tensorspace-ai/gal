// Gal — application shell, inbox and wave view.

import { Delta, compose } from './ot.js';
import { BlipDoc, Connection } from './client.js';
import { Editor, indexToDom, renderDelta } from './editor.js';
import {
  askFor,
  avatar,
  clear,
  clockTime,
  confirmAction,
  CursorLayer,
  dayLabel,
  el,
  fullTime,
  icon,
  ICONS,
  relativeTime,
  sameDay,
  shortClockTime,
  toast,
  userColor,
} from './ui.js';

// The cursor layer positions carets via the editor's index mapping.
window.__galEditorHelpers = { indexToDom };

/// Mirrors WaveMode in gal-core. The server is the authority; these are used to
/// choose a layout and to hide affordances that would only be refused.
const MODES = [
  { id: 'document', label: 'Document', hint: 'Everyone can edit every message. Replies nest.' },
  { id: 'chat', label: 'Chat', hint: 'A channel. Only you can edit your own messages.' },
  { id: 'announcement', label: 'Announcement', hint: 'Only you can post; anyone can reply.' },
  { id: 'notepad', label: 'Notepad', hint: 'One shared page that everyone edits.' },
  { id: 'frozen', label: 'Frozen', hint: 'Read-only. Nothing can change until you unfreeze it.' },
];

const mode = {
  current: () => (state.wave && state.wave.mode) || 'document',
  is: (...names) => names.includes(mode.current()),
  isFlat: () => mode.is('chat', 'notepad'),
  allowsReplies: () => mode.is('document', 'announcement'),
  allowsNewMessage: () =>
    mode.is('document', 'chat') || (mode.is('announcement') && isCreator()),
  allowsDelete: () => mode.is('document', 'chat', 'announcement'),
  allowsPrivateReply: () => mode.is('document', 'chat', 'announcement'),
  /// Can this user edit this blip? Mirrors WaveMode::allows_edit.
  allowsEdit: (blip) => {
    switch (mode.current()) {
      case 'document':
      case 'notepad':
        return true;
      case 'chat':
      case 'announcement':
        return blip.author === state.me.id;
      default:
        return false;
    }
  },
};

function isCreator() {
  return Boolean(state.wave && state.wave.creator === state.me.id);
}

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
  /// The chat composer, when the mode has one. Deliberately separate from
  /// `editors`, which renderThread() rebuilds wholesale.
  composer: null,
  remoteCursors: new Map(),
  cursorLayer: null,
  activeEditor: null,
  /// The one formatting toolbar, and where it sits when no editor has the
  /// caret. Both are rebuilt with the wave.
  toolbar: null,
  toolbarHome: null,
  /// Blips whose outstanding op the server has refused by name. The reopen
  /// that follows must not rescue and resubmit it: a refusal the mode check
  /// knows nothing about — an embed the server will not take, say — would be
  /// replayed, refused, and replayed again forever.
  doomed: new Set(),
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
        // Same shape as an inbox row, so the timestamp lines up in the same
        // place rather than dropping onto a line of its own.
        el('div', { class: 'inbox-row-top' }, [
          el('div', { class: 'inbox-title', text: hit.title }),
          el('div', { class: 'inbox-time', text: relativeTime(hit.timestamp) }),
        ]),
        // The snippet arrives pre-escaped from SQLite with <mark> highlights.
        el('div', { class: 'inbox-snippet', html: hit.snippet }),
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
  pane.classList.remove('chat');
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
  state.composer = null;
  state.toolbar = null;
  state.toolbarHome = null;
  state.doomed.clear();
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
  // Chat is laid out as a channel rather than a stack of cards; the switch is
  // made once here so the stylesheet can do the rest.
  pane.classList.toggle('chat', mode.is('chat'));

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
        title: 'Click to rename',
        onClick: async () => {
          const next = await askFor('Rename wave', { value: title, confirmLabel: 'Rename' });
          if (next && root) conn.send({ type: 'setTitle', waveletId: root.id, title: next });
        },
      }, [
        mode.is('chat') ? el('span', { class: 'channel-hash', text: '#' }) : null,
        el('span', { class: 'wave-title-text', text: title }),
      ]),
      el('div', { class: 'wave-sub' }, [
        el('span', { class: 'presence', id: 'presence' }),
        el('span', { class: 'participants', id: 'participants' }),
      ]),
    ]),
    el('div', { class: 'wave-actions' }, [
      modeControl(),
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
  const scroller = el('div', { class: 'thread-scroll' }, [thread]);

  pane.appendChild(header);
  pane.appendChild(scroller);
  // A sibling of the scroller, not a child of it: a composer that scrolls out
  // of reach is no use in a channel, and in the other modes "Add message"
  // should not need a trip to the bottom of a long document either.
  pane.appendChild(el('div', { class: 'thread-foot', id: 'thread-foot' }));

  state.cursorLayer = new CursorLayer(scroller);
  // Nothing here can be written to during playback, and a frozen wave refuses
  // every edit, so in both cases the controls would only ever be decoration.
  state.toolbar = state.playback || mode.is('frozen') ? null : buildToolbar();
  renderParticipants();
  renderPresence();
  renderThread();
  renderComposer();
}

/// What sits below the thread: a chat composer, an "add message" button, or an
/// explanation of why neither is offered.
///
/// Deliberately outside `#thread`. renderThread() clears and rebuilds that
/// element on every incoming message, so a composer inside it would lose its
/// caret each time anyone else typed.
function renderComposer() {
  const host = document.getElementById('thread-foot');
  if (!host) return;
  clear(host);
  // The old composer, if there was one, has just been detached along with the
  // rest of the footer. Anything still holding it would be writing into a node
  // that is no longer on the page.
  if (state.activeEditor === state.composer) state.activeEditor = null;
  state.composer = null;
  state.toolbarHome = null;
  const root = rootWavelet();
  if (!root || state.playback) return;

  // Where the formatting controls rest when no editor holds the caret. It is
  // always beside whatever this mode offers as an input, so the controls are
  // never further from the writing than the button that starts it.
  const tools = el('div', { class: 'foot-tools' });
  state.toolbarHome = state.toolbar ? tools : null;

  if (!mode.allowsNewMessage()) {
    const why = mode.is('frozen')
      ? 'This wave is frozen.'
      : mode.is('notepad')
        ? 'This wave is a single shared page — edit it directly above.'
        : 'Only the person who started this wave can post here.';
    host.appendChild(el('div', { class: 'foot-row' }, [
      tools,
      el('p', { class: 'compose-note', text: why }),
    ]));
    dockToolbar();
    return;
  }

  // Outside chat, a message is created empty and typed into in place.
  if (!mode.is('chat')) {
    host.appendChild(el('div', { class: 'foot-row' }, [
      tools,
      el('button', {
        class: 'btn primary',
        text: 'Add message',
        onClick: () => conn.send({ type: 'createBlip', waveletId: root.id }),
      }),
    ]));
    dockToolbar();
    return;
  }

  const field = el('div', { class: 'composer-input' });
  const box = el('div', { class: 'composer' }, [
    field,
    el('div', { class: 'composer-bar' }, [
      tools,
      el('button', {
        class: 'btn primary composer-send',
        text: 'Send',
        title: 'Enter',
        onMousedown: (e) => e.preventDefault(),
        onClick: () => send(),
      }),
    ]),
  ]);
  host.appendChild(box);

  // A standalone editor over a local draft. It is never registered in
  // state.docs or state.editors, so nothing that re-renders the thread can
  // touch it.
  const composer = new Editor(field, new Delta(), {
    onChange: () => {},
    onFiles: (files, at) => uploadInto(composer, files, at),
    // Without this the toolbar and ⌘B have nothing to act on while the only
    // thing being typed into is the composer.
    onSelectionChange: () => {
      state.activeEditor = composer;
      dockToolbar();
    },
  });
  composer.toolsHost = tools;
  composer.waveletId = root.id;
  state.composer = composer;

  const send = () => {
    const text = composer.doc.toPlainText().trim();
    if (!text) return;
    conn.send({ type: 'createBlip', waveletId: root.id, content: composer.doc });
    composer.reset(new Delta());
    composer.root.focus();
  };

  // Enter sends; Shift+Enter is a newline. Returning false tells the editor the
  // key was handled — and preventDefault is ours to call, or the browser still
  // inserts its own line break behind our back.
  composer.onEnter = (event) => {
    event.preventDefault();
    send();
    return false;
  };
  composer.root.setAttribute('data-placeholder', `Message #${root.title}`);
  dockToolbar();
}

/// The mode indicator. Only the creator can change it, so everyone else sees a
/// plain label explaining why the wave behaves as it does.
function modeControl() {
  const current = MODES.find((m) => m.id === mode.current()) || MODES[0];

  if (!isCreator()) {
    return el('span', {
      class: `mode-badge mode-${current.id}`,
      text: current.label,
      title: current.hint,
    });
  }

  const select = el('select', {
    class: `mode-select mode-${current.id}`,
    title: current.hint,
    onChange: async (e) => {
      const next = e.target.value;
      if (next === mode.current()) return;
      const chosen = MODES.find((m) => m.id === next);
      const ok = await confirmAction(
        `Switch to ${chosen.label}?`,
        `${chosen.hint}\n\nThis changes what everyone in the wave can do. Nothing is ` +
          `deleted, and you can switch back at any time.`,
        { confirmLabel: `Switch to ${chosen.label}` },
      );
      if (!ok) {
        e.target.value = mode.current();
        return;
      }
      conn.send({ type: 'setMode', waveId: state.waveId, mode: next });
    },
  });
  for (const m of MODES) {
    select.appendChild(
      el('option', { value: m.id, text: m.label, selected: m.id === current.id }),
    );
  }
  return select;
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

/// Consecutive messages from one author inside this window share a header.
const GROUP_WINDOW = 5 * 60 * 1000;

/// Whether the thread is showing its newest content. A little slack, so that
/// resting a few pixels short of the end still counts as being there.
function threadAtBottom() {
  const scroller = document.querySelector('.thread-scroll');
  if (!scroller) return false;
  return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
}

/** Build the blip tree, reusing existing nodes so editors keep their state. */
function renderThread() {
  const host = document.getElementById('thread');
  if (!host) return;

  // A channel reads from the bottom. Follow the newest message only when the
  // reader is already there — moving the viewport under someone who scrolled
  // up to read something older is worse than letting a message arrive
  // off-screen.
  const scroller = host.closest('.thread-scroll');
  const pinned = threadAtBottom();
  // Every node below is rebuilt, including the one being typed into. The
  // documents survive — they live in `state.docs` — so the caret can be put
  // back where it was, which is the difference between someone else adding a
  // message and someone else throwing you out of your sentence.
  const focused = captureFocus();

  clear(host);

  const blips = allBlips();
  const byParent = new Map();
  for (const blip of blips) {
    // Flat modes ignore `parent` when laying out. The server still stores it, so
    // switching back to a threaded mode restores the tree exactly.
    const key = (!mode.isFlat() && blip.parent) || `root:${blip.waveletId}`;
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

  const chat = mode.is('chat');

  const renderInto = (container, key, depth) => {
    // Only meaningful in chat, where messages arrive in one flat run: the
    // previous message decides whether this one repeats its header and whether
    // a new day has started. The two are tracked apart because a run of
    // messages can be broken without the day changing.
    let previous = null;
    let previousDay = null;

    for (const blip of byParent.get(key) || []) {
      if (chat && (previousDay === null || !sameDay(previousDay, blip.createdAt))) {
        container.appendChild(el('div', { class: 'day-sep' }, [
          el('span', { class: 'day-label', text: dayLabel(blip.createdAt) }),
        ]));
        previous = null;
      }
      const grouped = Boolean(
        chat &&
          previous &&
          previous.author === blip.author &&
          blip.createdAt - previous.createdAt < GROUP_WINDOW &&
          !carriesHeaderDetail(blip),
      );

      const node = blipElement(blip, depth, { grouped });
      container.appendChild(node);
      previous = blip;
      previousDay = blip.createdAt;

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
        // The aside breaks the run of messages, so the next one starts fresh.
        previous = null;
      }
    }
  };

  const root = rootWavelet();
  if (root) renderInto(host, `root:${root.id}`, 0);

  restoreFocus(focused);
  // The toolbar was docked inside one of the nodes just discarded. Without
  // this it stays in the detached subtree whenever the caret was not in an
  // editor — clicking the search box and then having someone post was enough
  // to take bold, links and the paperclip off the page entirely.
  dockToolbar();
  if (chat && pinned) scroller.scrollTop = scroller.scrollHeight;
}

/// Which message the caret is in, and where, before the thread is rebuilt.
///
/// The composer is deliberately not covered: it lives outside `#thread` and is
/// never rebuilt from here, so it keeps its caret on its own.
function captureFocus() {
  const editor = state.activeEditor;
  if (!editor || !editor.blipId || !editor.root.isConnected) return null;
  const active = document.activeElement;
  if (active !== editor.root && !editor.root.contains(active)) return null;
  return { blipId: editor.blipId, selection: editor.getSelection() };
}

function restoreFocus(saved) {
  if (!saved) return;
  const editor = state.editors.get(saved.blipId);
  if (!editor) return;
  editor.root.focus();
  if (saved.selection) editor.setSelection(saved.selection.index, saved.selection.length);
}

function blipElement(blip, depth, { grouped = false } = {}) {
  const author = lookupUser(blip.author);
  const chat = mode.is('chat');

  const doc = state.docs.get(blip.id) || new BlipDoc(blip.id, blip.content, blip.revision);
  state.docs.set(blip.id, doc);

  const body = el('div', { class: 'blip-body' });
  const article = el('article', {
    class: `blip ${chat ? 'chat' : ''} ${grouped ? 'grouped' : ''} ${blip.unread ? 'unread' : ''}`,
    dataset: { blipId: blip.id, depth: String(depth) },
  });

  const contributors = (blip.contributors || []).filter((id) => id !== blip.author);
  const actions = el('div', { class: 'blip-actions' }, [
    mode.allowsReplies() &&
      actionButton('Reply', 'Reply below this message', () => {
      conn.send({ type: 'createBlip', waveletId: blip.waveletId, parent: blip.id });
    }),
    mode.allowsPrivateReply() &&
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
    blip.author === state.me.id && mode.allowsDelete()
      ? actionButton('Delete', 'Delete this message', async () => {
          const ok = await confirmAction('Delete message', 'This cannot be undone.', {
            confirmLabel: 'Delete',
            danger: true,
          });
          if (ok) conn.send({ type: 'deleteBlip', blipId: blip.id });
        })
      : null,
  ]);

  const contributorTag = contributors.length
    ? el('span', {
        class: 'blip-contributors',
        title: 'Also edited by others',
        text: `+${contributors.length}`,
      })
    : null;

  if (chat) {
    // A channel row: the author's face in a gutter, the message beside it, and
    // repeated headers collapsed away so a run of messages reads as one turn.
    // The timestamp is when the message was sent — a chat is a record of what
    // was said when, so a later edit is marked rather than backdating the line.
    const sent = el('time', {
      class: 'blip-time',
      text: grouped ? shortClockTime(blip.createdAt) : clockTime(blip.createdAt),
      title: fullTime(blip.createdAt),
    });

    article.appendChild(el('div', { class: 'blip-gutter' }, [
      grouped ? sent : avatar(author, { size: 34 }),
    ]));
    article.appendChild(el('div', { class: 'blip-main' }, [
      grouped
        ? null
        : el('div', { class: 'blip-head' }, [
            el('span', { class: 'blip-author', text: author.displayName }),
            sent,
            wasEdited(blip)
              ? el('span', {
                  class: 'blip-edited',
                  text: 'edited',
                  title: `Last edited ${fullTime(blip.lastModified)}`,
                })
              : null,
            contributorTag,
          ]),
      body,
    ]));
    article.appendChild(actions);
  } else {
    article.appendChild(el('div', { class: 'blip-head' }, [
      avatar(author, { size: 26 }),
      el('span', { class: 'blip-author', text: author.displayName }),
      el('time', {
        class: 'blip-time',
        text: relativeTime(blip.lastModified),
        title: fullTime(blip.lastModified),
      }),
      contributorTag,
      actions,
    ]));
    article.appendChild(body);
  }
  article.style.setProperty('--author-color', userColor(author));

  const editorRoot = el('div', { class: 'editor' });
  body.appendChild(editorRoot);

  const editable = !state.playback && mode.allowsEdit(blip);
  if (!editable) {
    // Rendered rather than edited. The server refuses the op regardless; this
    // just avoids offering a caret that would go nowhere.
    editorRoot.contentEditable = 'false';
    editorRoot.classList.add('read-only');
    renderDelta(editorRoot, doc.doc);
  } else {
    // The formatting controls dock here while this message holds the caret.
    const tools = el('div', { class: 'blip-tools' });
    body.appendChild(tools);

    const editor = new Editor(editorRoot, doc.doc, {
      onChange: (delta) => onLocalEdit(blip.id, delta),
      onFiles: (files, at) => uploadInto(editor, files, at),
      onSelectionChange: (selection) => {
        state.activeEditor = editor;
        dockToolbar();
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
    editor.blipId = blip.id;
    editor.waveletId = blip.waveletId;
    editor.toolsHost = tools;
    state.editors.set(blip.id, editor);
  }

  state.blipNodes.set(blip.id, article);
  return article;
}

/// Resolve a user id to a profile for display.
///
/// The wave's own participant lists are the primary source: they always contain
/// everyone who could have written something here. `state.users` is only a
/// fallback, and cannot be relied on alone — it is fetched once at startup, and
/// a user who shared no waves at that moment gets an empty list, which used to
/// render their own messages as "Unknown".
function lookupUser(userId) {
  if (state.me && state.me.id === userId) return state.me;
  if (state.wave) {
    for (const wavelet of state.wave.wavelets) {
      const found = wavelet.participants.find((p) => p.id === userId);
      if (found) return found;
    }
  }
  return (
    state.users.find((u) => u.id === userId) || {
      id: userId,
      name: '?',
      displayName: 'Unknown',
      color: 0,
    }
  );
}

/// Whether a message has been changed since it was sent.
///
/// A blip's two timestamps are set together at creation and the content sent
/// with it is committed under the creation time, so anything later is a real
/// edit. The minute of slack covers a message that was created empty and typed
/// into — how every mode but chat writes one, and what a wave switched into
/// chat is full of.
function wasEdited(blip) {
  return blip.lastModified - blip.createdAt > 60 * 1000;
}

/// Whether a message's header says more than "who, and when".
///
/// Folding a message into the run above it drops the header, which is the point
/// — but it must not be a way to lose the mark saying a message was edited, or
/// the names of the other people on it. A wave that spent time in Document mode
/// and was then switched to Chat is full of both.
function carriesHeaderDetail(blip) {
  return wasEdited(blip) || (blip.contributors || []).some((id) => id !== blip.author);
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
    // Would be refused on arrival, and resubmitting on every reconnect would
    // loop forever.
    const blip = allBlips().find((b) => b.id === blipId);
    if (blip && !mode.allowsEdit(blip)) {
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

/**
 * Build the wave's one set of formatting controls.
 *
 * There is exactly one, and it is moved to whichever input holds the caret
 * rather than copied into each of them. It used to sit in the wave's header,
 * which put it a long way from the text it acts on — in a channel, the whole
 * height of the transcript away from the composer.
 */
function buildToolbar() {
  const bar = el('div', { class: 'toolbar' });
  for (const [name, label, title] of FORMATS) {
    bar.appendChild(el('button', {
      class: `tool tool-${name}`,
      type: 'button',
      text: label,
      title,
      'aria-label': title.split('  ')[0],
      // Keep focus in the editor so the selection survives the click.
      onMousedown: (e) => e.preventDefault(),
      onClick: () => applyFormat(name),
    }));
  }
  bar.appendChild(el('button', {
    class: 'tool tool-link',
    type: 'button',
    title: 'Add a link',
    'aria-label': 'Add a link',
    onMousedown: (e) => e.preventDefault(),
    onClick: async () => {
      const editor = state.activeEditor;
      if (!editor) return;
      const url = await askFor('Link', { placeholder: 'https://…', confirmLabel: 'Link' });
      if (url) editor.format('link', url);
      updateToolbar();
    },
  }, [icon(ICONS.link)]));

  // Hidden, and clicked by the button beside it: a bare file input cannot be
  // styled to match anything.
  const picker = el('input', {
    class: 'file-picker',
    type: 'file',
    multiple: true,
    onChange: (event) => {
      const files = Array.from(event.target.files || []);
      // Cleared so that choosing the same file twice running still fires.
      event.target.value = '';
      const editor = bar.pendingEditor || state.activeEditor;
      const at = bar.pendingSelection;
      bar.pendingEditor = null;
      bar.pendingSelection = null;
      if (files.length > 0) uploadInto(editor, files, at);
    },
  });

  bar.appendChild(el('button', {
    class: 'tool tool-attach',
    type: 'button',
    title: 'Attach a file',
    'aria-label': 'Attach a file',
    onMousedown: (e) => e.preventDefault(),
    onClick: () => {
      const editor = state.activeEditor;
      if (!editor || !editor.waveletId) {
        toast('Put the caret in a message first.');
        return;
      }
      // Opening the picker takes the focus and the upload takes a moment, so
      // where the file should land is decided now rather than on the way back.
      bar.pendingEditor = editor;
      bar.pendingSelection = editor.getSelection();
      picker.click();
    },
  }, [icon(ICONS.paperclip)]));
  bar.appendChild(picker);
  return bar;
}

// --- attachments --------------------------------------------------------

async function uploadFile(waveletId, file) {
  const path =
    `/api/wavelets/${encodeURIComponent(waveletId)}/attachments` +
    `?name=${encodeURIComponent(file.name || 'file')}`;
  // The body is the file itself. What the browser calls it is a hint the server
  // does not trust — it identifies images by their bytes.
  const response = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': file.type || 'application/octet-stream' },
    body: file,
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || 'That file could not be uploaded.');
  return { id: body.id, name: body.name, mime: body.mime, size: body.size };
}

/**
 * Upload files and embed them in `editor`, starting at `at`.
 *
 * One at a time, so several files land in the order they were chosen rather
 * than in the order the network happened to finish them.
 */
async function uploadInto(editor, files, at) {
  if (!editor || !editor.waveletId) return;
  const bar = state.toolbar;
  if (bar) bar.classList.add('busy');
  let selection = at || editor.getSelection();

  try {
    for (const file of files) {
      const attachment = await uploadFile(editor.waveletId, file);
      // The editor may have been rebuilt underneath us while the bytes were in
      // flight; writing into a detached node would lose the file silently.
      const target = editor.blipId ? state.editors.get(editor.blipId) : editor;
      if (!target || !target.root.isConnected) {
        toast('That message went away while the file was uploading.', 'error');
        return;
      }
      target.insertEmbed({ attachment }, selection);
      // The next file goes after this one, not on top of it.
      selection = selection ? { index: selection.index + 1, length: 0 } : null;
    }
  } catch (e) {
    // A refusal is nearly always the size limit or the daily quota, and the
    // rest of the batch would be refused for the same reason.
    toast(e.message, 'error');
  } finally {
    if (bar) bar.classList.remove('busy');
  }
}

/**
 * Move the toolbar to the input that has the caret, or back to its home beside
 * the composer when none does.
 *
 * Re-parenting rather than showing and hiding several copies means the active
 * states below are computed once, and there is never a second toolbar lit up
 * for a selection it does not own.
 */
function dockToolbar() {
  const bar = state.toolbar;
  if (!bar) return;
  const editor = state.activeEditor;
  const host =
    (editor && editor.toolsHost && editor.toolsHost.isConnected && editor.toolsHost) ||
    state.toolbarHome;
  if (!host || !host.isConnected) {
    bar.remove();
    return;
  }
  if (bar.parentNode !== host) host.appendChild(bar);
  updateToolbar();
}

function applyFormat(name) {
  const editor = state.activeEditor;
  if (!editor) return;
  editor.format(name, editor.isActive(name) ? null : true);
  updateToolbar();
}

function updateToolbar() {
  const bar = state.toolbar;
  if (!bar) return;
  const editor = state.activeEditor;
  // Resting beside the composer with nothing to act on yet. Dimmed rather than
  // hidden: it is how you find out the formatting is there at all, and it
  // should not claim to be usable before there is a caret.
  bar.classList.toggle('idle', !editor);
  for (const [name] of FORMATS) {
    const button = bar.querySelector(`.tool-${name}`);
    // Without the `editor` guard the buttons keep whatever they were showing
    // for an editor that is no longer being written to.
    if (button) button.classList.toggle('active', Boolean(editor) && editor.isActive(name));
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
      // Work the server has already refused by name is dropped here rather
      // than rescued. replayUnsentEdits only knows how to recognise a mode
      // refusal, so anything else would be resubmitted, refused, and
      // resubmitted again for as long as the tab stays open.
      if (state.doomed.has(blipId)) continue;
      const work = doc.pendingWork();
      if (work && !work.isEmpty()) unsent.set(blipId, work);
    }
    state.doomed.clear();
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

  // In a channel the caret belongs in the composer. Elsewhere, put it in the
  // first empty message, which is where a new wave lands.
  if (state.composer) {
    state.composer.root.focus();
  } else {
    const empty = allBlips().find((b) => (b.content.ops || []).length === 0);
    if (empty) {
      const editor = state.editors.get(empty.id);
      if (editor) editor.root.focus();
    }
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

  // A message that lands while you are looking at the newest part of the wave
  // has been read by the time it is drawn. Without this the inbox goes on
  // counting messages that are on the screen in front of you, and a channel
  // ends up with everything said since it was opened marked as new. Read from
  // the DOM before the render below moves the scroller.
  const watched = document.visibilityState === 'visible' && threadAtBottom();
  if (watched) message.blip.unread = false;

  wavelet.blips.push(message.blip);
  renderThread();
  if (watched) conn.send({ type: 'markRead', waveId: state.waveId });

  // Focus a message we just created ourselves — but never in a channel, where
  // a message is *sent*, not created empty and filled in. There the caret
  // belongs in the composer, and pulling it into the message that just left
  // meant the next thing typed silently edited the message already sent.
  if (message.blip.author === state.me.id && !state.composer) {
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

conn.on('modeChanged', (message) => {
  if (!state.wave || message.waveId !== state.waveId) return;
  state.wave.mode = message.mode;

  // Count unsent edits the new mode will no longer accept. Keeping them would
  // mean retrying an op the server refuses for as long as the mode lasts.
  let abandoned = 0;
  for (const wavelet of state.wave.wavelets) {
    for (const blip of wavelet.blips) {
      const doc = state.docs.get(blip.id);
      if (!doc || mode.allowsEdit(blip)) continue;
      const pending = doc.pendingWork();
      if (pending && !pending.isEmpty()) abandoned += 1;
    }
  }

  renderWave();

  if (abandoned > 0) {
    toast(
      `This wave is now ${message.mode}. ${abandoned} unsent ` +
        `${abandoned === 1 ? 'change was' : 'changes were'} discarded.`,
      'error',
    );
    // Reopen for authoritative content. Note what we must NOT do: reset from
    // the copy of the blip held in `state.wave`. That is the snapshot taken when
    // the wave was opened, and every edit since has arrived as an operation, so
    // adopting it would silently discard everything typed in this session.
    // replayUnsentEdits skips blips the mode forbids, so the doomed work is
    // dropped and everything else is replayed.
    conn.openWave(state.waveId);
  } else {
    toast(`This wave is now ${message.mode}.`);
  }
});

conn.on('titleChanged', (message) => {
  if (state.wave && message.waveId === state.waveId) {
    const wavelet = state.wave.wavelets.find((w) => w.id === message.waveletId);
    if (wavelet) wavelet.title = message.title;
    // The text node only: in chat the heading also carries the channel's hash.
    const heading = document.querySelector('.wave-title-text');
    if (heading) heading.textContent = message.title;
    if (state.composer) {
      state.composer.root.setAttribute('data-placeholder', `Message #${message.title}`);
    }
  }
});

conn.on('participantAdded', (message) => {
  if (!state.users.some((u) => u.id === message.user.id)) state.users.push(message.user);
  // Keeps the picker's suggestions current without another round trip.
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
  // A refused edit names the message it applies to. Reset that document to the
  // server's version: retrying is pointless while the reason stands, and
  // holding the op would block everything typed afterwards.
  //
  // Any named refusal, not just a mode refusal: an op the server will not take
  // is an op the client must stop holding, whatever the reason was.
  if (message.blipId && message.code !== 'resync') {
    toast(message.message, 'error');
    // Reopen rather than reconstructing locally: the client's copy of the blip
    // is the snapshot from when the wave was opened, so it is not a safe source
    // of truth. Marking it doomed first is what makes the reopen drop the op
    // rather than replay it — the reopen alone only sheds work the *mode*
    // forbids, so any other refusal came back around and was sent again.
    if (state.docs.get(message.blipId)?.pendingWork()) {
      state.doomed.add(message.blipId);
      conn.openWave(state.waveId);
    }
    return;
  }
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
