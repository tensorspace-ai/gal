// Gal — application shell, inbox and wave view.

import { Delta, compose } from './ot.js';
import { BlipDoc, Connection } from './client.js';
import { COMMENT_ATTR, commentRanges, Editor, indexToDom, renderDelta } from './editor.js';
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
  {
    id: 'notepad',
    label: 'Notepad',
    hint: 'One shared page that everyone edits, with comments in the margin.',
  },
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
  /// Mirrors WaveMode::allows_comments — a notepad's only way to say something
  /// about the page rather than in it.
  allowsComments: () => mode.is('notepad'),
  /// Wider than allowsComments on purpose, matching WaveMode::allows_resolve:
  /// leaving Notepad must not strand threads that nothing could then close.
  allowsResolve: () => !mode.is('frozen'),
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
  /// The margin the comment cards sit in, rebuilt with the wave.
  commentRail: null,
  /// The thread whose card and highlight are lit up, if any.
  activeComment: null,
  /// A message a search hit is on its way to, held until the thread has been
  /// built from the snapshot and the node exists to scroll to.
  revealBlip: null,
  /// Whether settled threads are on show. Off by default: the point of
  /// resolving one is to get it out of the margin.
  showResolved: false,
  /// Half-typed replies, by thread, so rebuilding the margin does not throw
  /// away a sentence someone is in the middle of.
  commentDrafts: new Map(),
  commentReplyFocused: false,
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
        text: 'Sessions',
        title: 'Sign out every other browser and device',
        onClick: signOutEverywhere,
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

  // Above the layout rather than inside the sidebar. The whole account a person
  // got of being offline was a 7px dot in the sidebar footer, with the
  // explanation in a `title` — and below 860px the sidebar is hidden entirely
  // while a wave is open, so on a phone there was no indication at all.
  app.appendChild(el('div', {
    class: 'offline-banner',
    id: 'offline-banner',
    role: 'status',
    'aria-live': 'polite',
    hidden: true,
  }));
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

/**
 * What a wave counts as unread for. Muting a wave is a statement that you do
 * not want to be told about it, so it keeps its messages and loses its claim on
 * your attention: no badge, and out of the Unread filter. It stays in All,
 * because muting is not archiving and the conversation is still yours.
 */
function unreadCount(row) {
  return row.flags.muted ? 0 : row.unreadCount;
}

function inboxRows() {
  const rows = [...state.inbox.values()];
  const filtered = rows.filter((row) => {
    if (state.filter === 'unread') return unreadCount(row) > 0 && !row.flags.archived;
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
        onClick: () => openWave(hit.waveId, { revealBlip: hit.blipId }),
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

    const unread = unreadCount(row);
    host.appendChild(el('button', {
      class: `inbox-row ${row.id === state.waveId ? 'active' : ''} ${unread ? 'unread' : ''}`
        + `${row.flags.muted ? ' muted' : ''}`,
      onClick: () => openWave(row.id),
      'aria-current': row.id === state.waveId ? 'true' : null,
    }, [
      el('div', { class: 'inbox-row-top' }, [
        faces,
        el('div', { class: 'inbox-title', text: row.title }),
        row.flags.muted
          ? el('span', { class: 'muted-mark', text: 'Muted', title: 'Muted — no unread count' })
          : null,
        el('div', { class: 'inbox-time', text: relativeTime(row.lastModified) }),
      ]),
      el('div', { class: 'inbox-snippet', text: row.snippet || 'No messages yet' }),
      unread
        ? el('span', { class: 'badge', text: String(unread) })
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
  if (dot) {
    dot.className = `status-dot ${status}`;
    dot.title = status === 'online' ? 'Connected' : 'Reconnecting…';
  }

  const banner = document.getElementById('offline-banner');
  if (!banner) return;
  const offline = status !== 'online';
  banner.hidden = !offline;
  // Rewritten rather than left in place, so a screen reader announces the
  // change instead of a region that was already saying this.
  banner.textContent = offline
    ? 'Offline — reconnecting. You can keep typing; your changes will be sent.'
    : '';

  // The banner is fixed, so the layout has to give up the space or it is drawn
  // over the header — over the wave's own title and buttons, and over the
  // sidebar's. Measured rather than assumed: on a narrow screen the sentence
  // wraps to two lines and a hardcoded height would still cover something.
  //
  // Rounded up from the fractional rect rather than taken from offsetHeight,
  // which is an integer: a banner 33.375px tall reserved 33 and covered the
  // header by the difference.
  const layout = document.querySelector('.layout');
  const height = Math.ceil(banner.getBoundingClientRect().height);
  if (layout) layout.style.paddingTop = offline ? `${height}px` : '';
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

function openWave(waveId, { revealBlip = null } = {}) {
  if (state.waveId === waveId) {
    // Already here: a second search hit in the open wave still has somewhere to
    // take you, so do the reveal rather than nothing.
    if (revealBlip) revealBlip_(revealBlip);
    return;
  }
  if (state.waveId) conn.closeWave(state.waveId);
  teardownWave();
  state.waveId = waveId;
  // Held until the thread has been built from the snapshot; the blip does not
  // exist as a node yet.
  state.revealBlip = revealBlip;
  conn.openWave(waveId);
  showInbox(false);
  renderInbox();
}

/**
 * Scroll a message into view and mark it briefly.
 *
 * Search has always known which message matched — `blipId` is on every hit —
 * and the client threw it away and opened the wave at the top, leaving you to
 * find the line again by eye in a conversation that might be a year long.
 */
function revealBlip_(blipId) {
  const node = state.blipNodes.get(blipId);
  if (!node) return false;
  node.scrollIntoView({ block: 'center', behavior: 'smooth' });
  node.classList.remove('revealed');
  // Restart the animation: without the reflow a second hit on the same message
  // re-adds a class the node already has and nothing happens.
  void node.offsetWidth;
  node.classList.add('revealed');
  return true;
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
  state.revealBlip = null;
  state.commentRail = null;
  state.activeComment = null;
  state.showResolved = false;
  state.commentReplyFocused = false;
  // Drafts belong to the wave being left, not to the next one.
  state.commentDrafts.clear();
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
      // A real button inside the heading rather than a click handler on the
      // heading itself: an h1 is not focusable and does not take Enter, so
      // renaming a wave was reachable with a mouse and by no other means.
      el('h1', { class: 'wave-title' }, [
        el('button', {
          class: 'wave-title-button',
          title: 'Rename this wave',
          onClick: async () => {
            const next = await askFor('Rename wave', { value: title, confirmLabel: 'Rename' });
            if (next && root) conn.send({ type: 'setTitle', waveletId: root.id, title: next });
          },
        }, [
          mode.is('chat') ? el('span', { class: 'channel-hash', text: '#' }) : null,
          el('span', { class: 'wave-title-text', text: title }),
        ]),
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
      // Muting keeps the wave and drops its claim on your attention. Archiving
      // is the one that takes it out of the list, and a wave that anybody
      // writes in comes straight back out of the archive — so without this
      // there was no way to stay in a busy conversation without being counted
      // at by it.
      el('button', {
        class: 'btn ghost',
        text: state.wave.flags.muted ? 'Unmute' : 'Mute',
        title: state.wave.flags.muted
          ? 'Count unread messages in this wave again'
          : 'Keep this wave, but stop counting its unread messages',
        onClick: () => conn.send({
          type: 'setFlags',
          waveId: state.waveId,
          flags: { ...state.wave.flags, muted: !state.wave.flags.muted },
        }),
      }),
      el('button', {
        class: 'btn ghost',
        text: 'Leave',
        title: 'Remove yourself from this wave',
        onClick: () => leaveWave(),
      }),
    ]),
  ]);

  const thread = el('div', { class: 'thread', id: 'thread' });
  // A sibling of the thread rather than a child: the cards are positioned
  // against the page's text but must not be part of its flow.
  const rail = el('div', { class: 'comment-rail', id: 'comment-rail' });
  const scroller = el('div', { class: 'thread-scroll' }, [thread, rail]);
  state.commentRail = rail;

  // Clicking a highlighted phrase brings up its thread. Delegated, because the
  // marks are rebuilt on every render and every remote edit.
  thread.addEventListener('click', (event) => {
    const mark = event.target.closest && event.target.closest('.commented');
    if (mark && mark.dataset.comment) setActiveComment(mark.dataset.comment);
  });

  // A card slides to its new position, and someone else's caret may be inside
  // it. The layout pass redraws the carets, but it does so before the slide has
  // happened — so they have to be redrawn again when it has.
  rail.addEventListener('transitionend', (event) => {
    if (event.propertyName === 'top' && state.cursorLayer) state.cursorLayer.render();
  });

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

/**
 * Remove yourself from the open wave.
 *
 * The server takes the private replies with it, so say so: someone leaving a
 * wave they had a side conversation in is leaving that too, and finding out
 * afterwards is not the moment to learn it.
 */
async function leaveWave() {
  const root = rootWavelet();
  if (!root) return;
  // The server keeps a wave from being emptied, since nothing could ever reach
  // it again. Say so here rather than sending a request that comes back as an
  // error the person cannot act on.
  if (root.participants.length <= 1) {
    toast('You are the only person in this wave, and a wave needs someone in it.'
      + ' Archive it instead.');
    return;
  }
  const ok = await confirmAction(
    'Leave this wave',
    'You will stop receiving it, including any private replies you are part of.'
      + ' Someone still in it can add you back.',
    { confirmLabel: 'Leave', danger: true },
  );
  if (!ok) return;
  conn.send({ type: 'removeParticipant', waveletId: root.id, userId: state.me.id });
}

function renderParticipants() {
  const host = document.getElementById('participants');
  if (!host) return;
  clear(host);
  const root = rootWavelet();
  if (!root) return;

  for (const participant of root.participants) {
    const isMe = participant.id === state.me.id;
    const label = isMe ? 'Leave this wave' : `Remove ${participant.displayName}`;
    // Wrapped in a button rather than given a click handler on the avatar span,
    // which was not focusable and took no key: managing who is in a wave was a
    // mouse-only capability.
    const face = el('button', {
      class: 'participant-button',
      title: label,
      'aria-label': label,
    }, [avatar(participant, { size: 24 })]);
    const act = async () => {
      // Clicking your own face used to do nothing at all, so there was no way
      // out of a wave from the client even though the server has always allowed
      // anyone to remove themselves.
      if (isMe) {
        await leaveWave();
        return;
      }
      const ok = await confirmAction(
        'Remove participant',
        `Remove ${participant.displayName} from this wave?`,
        { confirmLabel: 'Remove', danger: true },
      );
      if (ok) {
        conn.send({ type: 'removeParticipant', waveletId: root.id, userId: participant.id });
      }
    };
    face.addEventListener('click', act);
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

  // Remarks are blips too, but they belong beside the page rather than in it.
  // They are laid out by renderComments() instead.
  const blips = allBlips().filter((b) => !b.comment);
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

  // Before restoring the caret, not after: the cards hang off the text that was
  // just rebuilt, so their anchors and their editors are rebuilt with it — and a
  // caret restored into a remark first would be dropped again the moment the
  // margin replaced the editor holding it.
  renderComments();
  restoreFocus(focused);
  // The toolbar was docked inside one of the nodes just discarded. Without
  // this it stays in the detached subtree whenever the caret was not in an
  // editor — clicking the search box and then having someone post was enough
  // to take bold, links and the paperclip off the page entirely.
  dockToolbar();
  restoreCommentFocus();
  if (chat && pinned) scroller.scrollTop = scroller.scrollHeight;

  // A search hit waiting for its message to exist. Cleared once it lands, so a
  // later rebuild does not drag the reader back to it.
  if (state.revealBlip && revealBlip_(state.revealBlip)) state.revealBlip = null;
}

/// Put the caret back in the active card's reply box after a rebuild.
///
/// The page's own editors are covered by captureFocus/restoreFocus, which key
/// off a blip id; a reply box is a local draft and has none.
function restoreCommentFocus() {
  if (!state.commentReplyFocused || !state.commentRail) return;
  const box = state.commentRail.querySelector('.comment-card.active .comment-compose');
  if (!box || !box.replyEditor) return;
  box.replyEditor.root.focus();
  box.replyEditor.setSelection(box.replyEditor.doc.toPlainText().length);
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

// --- comments -----------------------------------------------------------
//
// A comment is a remark about a range of the page rather than about the wave.
// Where it points is not stored anywhere: the range is marked in the document
// itself with a `comment` attribute, so it is transformed by the same code that
// transforms bold, and the highlight follows the words as everyone edits around
// them. Everything below therefore *derives* positions from the document each
// time rather than remembering them.

/// Every comment thread the viewer can see. The server has already dropped the
/// ones belonging to wavelets they are not in.
function allComments() {
  if (!state.wave) return [];
  return state.wave.wavelets.flatMap((w) => w.comments || []);
}

/// The remarks of every thread, oldest first, in one pass.
///
/// Built once per render rather than per card: walking every blip in the wave
/// for each thread made drawing the margin cost the square of what is in it.
function remarksByComment() {
  const byThread = new Map();
  for (const blip of allBlips()) {
    if (!blip.comment) continue;
    const list = byThread.get(blip.comment);
    if (list) list.push(blip);
    else byThread.set(blip.comment, [blip]);
  }
  for (const list of byThread.values()) {
    list.sort((a, b) => a.seq - b.seq || a.createdAt - b.createdAt || (a.id < b.id ? -1 : 1));
  }
  return byThread;
}

/// Where each thread is anchored, read out of the live documents.
///
/// Deliberately from `state.docs` and not from `state.wave`: the latter is the
/// snapshot taken when the wave was opened, and every edit since — including the
/// ones that moved these ranges — arrived as an operation.
function anchorsByComment() {
  const anchors = new Map();
  for (const blip of allBlips()) {
    if (blip.comment) continue; // a remark, not the page
    const doc = state.docs.get(blip.id);
    if (!doc) continue;
    for (const range of commentRanges(doc.doc)) {
      const found = anchors.get(range.id);
      if (!found) {
        anchors.set(range.id, { blipId: blip.id, index: range.index, length: range.length });
      } else if (found.blipId === blip.id) {
        // One thread can end up as several runs — pasting into the middle of a
        // commented phrase will do it. Span them, so the card sits beside the
        // whole phrase instead of beside whichever fragment came first.
        const end = Math.max(found.index + found.length, range.index + range.length);
        found.index = Math.min(found.index, range.index);
        found.length = end - found.index;
      }
    }
  }
  return anchors;
}

/// The screen rectangle covering an anchored range, or null if it is not drawn.
function anchorRect(anchor) {
  const node = state.blipNodes.get(anchor.blipId);
  const root = node && node.querySelector('.editor');
  if (!root || !root.isConnected) return null;
  try {
    const from = indexToDom(root, anchor.index);
    const to = indexToDom(root, anchor.index + anchor.length);
    if (!from || !to) return null;
    const range = document.createRange();
    range.setStart(from.node, from.offset);
    range.setEnd(to.node, to.offset);
    const rect = range.getBoundingClientRect();
    return rect.height ? rect : null;
  } catch {
    // A range that cannot be built is a range that cannot be pointed at. The
    // thread still shows, as detached.
    return null;
  }
}

/**
 * Bring a thread forward: light its card and its highlight, and move the reply
 * box to it.
 *
 * Deliberately not a rebuild of the margin. A click that activates a card has
 * usually landed *in* one of its remarks, and rebuilding would replace the
 * editor the click was on its way into — so the caret would never arrive and
 * the remark would look uneditable.
 */
function setActiveComment(commentId) {
  if (state.activeComment === commentId) return;
  state.activeComment = commentId;
  state.commentReplyFocused = false;

  const rail = state.commentRail;
  if (!rail) return;
  const threads = allComments();
  for (const card of rail.querySelectorAll('.comment-card')) {
    const active = card.dataset.comment === commentId;
    card.classList.toggle('active', active);

    const box = card.querySelector('.comment-compose');
    if (box && !active) {
      if (box.replyEditor) box.replyEditor.destroy();
      box.remove();
    }
    if (active && !box) {
      const thread = threads.find((t) => t.id === commentId);
      if (thread && !thread.resolvedBy && mode.allowsComments()) {
        card.appendChild(commentReply(thread));
      }
    }
  }
  markAnchors(threads);
  layoutComments();
}

/// Start a thread on whatever is selected.
function startComment() {
  const editor = state.activeEditor;
  if (!editor || !editor.blipId || !editor.waveletId) {
    toast('Select some text on the page first.');
    return;
  }
  // Comments annotate the page, not each other; the server refuses this too.
  // Catching it here is what keeps the caret from being in a remark — where it
  // lands as soon as a thread is opened — and the button meaning something else.
  if (allBlips().some((b) => b.id === editor.blipId && b.comment)) {
    toast('Select some text on the page first.');
    return;
  }
  const selection = editor.getSelection();
  if (!selection || selection.length === 0) {
    toast('Select the words you want to comment on.');
    return;
  }
  // One comment per range: the anchor is a single attribute, so a second id
  // over the same words would overwrite the first and silently detach it.
  //
  // Tested by overlap rather than with `attributesAt`, which intersects across
  // the selection and so reports *nothing* for a range that covers a commented
  // phrase plus a word either side — the case that would do the overwriting.
  // Only threads this client knows about count: an anchor naming a thread that
  // does not exist is already invisible, and must not lock the words under it
  // out of ever being commented on.
  const known = new Set(allComments().map((t) => t.id));
  const end = selection.index + selection.length;
  const clash = commentRanges(editor.doc).find(
    (r) => known.has(r.id) && r.index < end && selection.index < r.index + r.length,
  );
  if (clash) {
    toast('That text already has a comment.');
    setActiveComment(clash.id);
    return;
  }

  const commentId = newCommentId();
  conn.send({
    type: 'createComment',
    waveletId: editor.waveletId,
    blipId: editor.blipId,
    commentId,
    content: new Delta(),
  });
  // Marked straight away, against the revision the selection was made in. There
  // is nothing to wait for: this is an ordinary op, so if someone else is typing
  // in the page the server rebases it exactly as it rebases everything else.
  // Waiting for the thread to be confirmed would mean re-finding a range that
  // had moved in the meantime.
  editor.format(COMMENT_ATTR, commentId);
  state.activeComment = commentId;
  state.commentReplyFocused = true;
}

/// An id in the shape the server accepts — see `CommentId::is_well_formed`.
///
/// Minted here rather than by the server so the anchor and the thread it names
/// can be created against the same revision.
function newCommentId() {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return `c-${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;
}

/**
 * Build the margin: one card per open thread, beside the words it is about.
 *
 * Split from `layoutComments` because this rebuilds the cards, and a card being
 * replied to holds a half-typed draft. Anything that only *moves* the text calls
 * the layout pass alone.
 */
function renderComments() {
  const rail = state.commentRail;
  if (!rail) return;
  clear(rail);

  // A thread whose blip has gone — deleted along with the message it annotated
  // — has nothing left to be about. The rows are removed server-side too; this
  // covers the moment before the next snapshot.
  const pages = new Set(allBlips().filter((b) => !b.comment).map((b) => b.id));
  const threads = allComments().filter((t) => pages.has(t.blipId));
  const anchors = anchorsByComment();
  const pane = document.getElementById('wave-pane');
  const resolvedCount = threads.filter((t) => t.resolvedBy).length;
  const shown = threads.filter((t) => state.showResolved || !t.resolvedBy);

  // Every remark gets a document, including those in threads that are not on
  // show. `state.docs` is what the `op` handler applies incoming edits to, so a
  // remark without one has its edits dropped on the floor — and is then built
  // from the stale snapshot content when its thread is finally revealed, so the
  // next thing typed into it is submitted against a revision the server left
  // behind long ago.
  const remarks = remarksByComment();
  for (const list of remarks.values()) {
    for (const blip of list) {
      if (!state.docs.has(blip.id)) {
        state.docs.set(blip.id, new BlipDoc(blip.id, blip.content, blip.revision));
      }
    }
  }

  if (pane) pane.classList.toggle('has-comments', threads.length > 0);
  markAnchors(threads);
  if (threads.length === 0) return;

  if (resolvedCount > 0) {
    rail.appendChild(el('button', {
      class: 'btn link comment-toggle',
      text: state.showResolved
        ? `Hide ${resolvedCount} resolved`
        : `Show ${resolvedCount} resolved`,
      onClick: () => {
        state.showResolved = !state.showResolved;
        renderComments();
      },
    }));
  }

  for (const thread of shown) {
    rail.appendChild(commentCard(thread, anchors.get(thread.id) || null, remarks));
  }
  layoutComments();
}

/// Tell the highlights in the page which threads they belong to.
///
/// A mark whose thread the client does not know about gets no highlight: that
/// is what an anchor looks like in the moment between marking the text and the
/// thread being confirmed, and what it would look like for good if the server
/// refused the thread. Better to show nothing than a highlight opening onto it.
function markAnchors(threads) {
  const byId = new Map(threads.map((t) => [t.id, t]));
  for (const mark of document.querySelectorAll('.commented')) {
    const thread = byId.get(mark.dataset.comment);
    mark.classList.toggle('orphan', !thread);
    mark.classList.toggle('resolved', Boolean(thread && thread.resolvedBy));
    mark.classList.toggle('active', Boolean(thread) && thread.id === state.activeComment);
  }
}

function commentCard(thread, anchor, remarksByThread) {
  const author = lookupUser(thread.author);
  const remarks = remarksByThread.get(thread.id) || [];
  const active = state.activeComment === thread.id;
  const resolved = Boolean(thread.resolvedBy);

  const card = el('article', {
    class: `comment-card ${active ? 'active' : ''} ${resolved ? 'resolved' : ''} ${
      anchor ? '' : 'detached'
    }`,
    dataset: { comment: thread.id },
    onClick: () => setActiveComment(thread.id),
  });

  const head = el('div', { class: 'comment-head' }, [
    avatar(author, { size: 22 }),
    el('span', { class: 'comment-author', text: author.displayName }),
    el('time', {
      class: 'comment-time',
      text: relativeTime(thread.createdAt),
      title: fullTime(thread.createdAt),
    }),
  ]);
  card.appendChild(head);

  // Always present, shown by the stylesheet only while the card is detached.
  // Whether it is detached can change with any keystroke in the page, and the
  // layout pass — which runs on every one — must not have to rebuild the card
  // to say so, or a reply being typed would be lost each time.
  //
  // The remarks are kept either way. Losing the discussion of why a sentence
  // was wrong, because someone acted on it, is the opposite of the point.
  card.appendChild(el('p', {
    class: 'comment-detached',
    text: 'The text this was about has been edited away.',
  }));

  remarks.forEach((remark, i) => card.appendChild(commentRemark(remark, i === 0)));

  const actions = el('div', { class: 'comment-actions' });
  if (mode.allowsResolve()) {
    actions.appendChild(el('button', {
      class: 'btn link',
      text: resolved ? 'Reopen' : 'Resolve',
      onClick: (e) => {
        e.stopPropagation();
        conn.send({ type: 'resolveComment', commentId: thread.id, resolved: !resolved });
      },
    }));
  }
  if (resolved && thread.resolvedBy) {
    actions.appendChild(el('span', {
      class: 'comment-settled',
      text: `Resolved by ${lookupUser(thread.resolvedBy).displayName}`,
    }));
  }
  card.appendChild(actions);

  // Only the card being read gets a reply box, and only while the thread is
  // open. Giving every card one would put a dozen editors on the page and make
  // the margin unreadable.
  if (active && !resolved && mode.allowsComments()) {
    card.appendChild(commentReply(thread));
  }
  return card;
}

/// One remark, editable by whoever the mode allows — it is a blip like any
/// other, so it gets the same live co-editing as the page.
function commentRemark(blip, first) {
  const author = lookupUser(blip.author);
  const doc = state.docs.get(blip.id) || new BlipDoc(blip.id, blip.content, blip.revision);
  state.docs.set(blip.id, doc);

  const editorRoot = el('div', { class: 'editor comment-text' });
  const row = el('div', { class: 'comment-remark' }, [
    // The card's own header already names whoever started the thread, and the
    // first remark is theirs by construction. Repeating it reads as two people.
    first
      ? null
      : el('div', { class: 'comment-remark-head' }, [
          el('span', { class: 'comment-author', text: author.displayName }),
          el('time', {
            class: 'comment-time',
            text: relativeTime(blip.lastModified),
            title: fullTime(blip.lastModified),
          }),
        ]),
    editorRoot,
  ]);

  // Registered like any other blip's node. That is what lets playback rewind a
  // remark's text along with the page, and what lets a remote edit reach one
  // that is rendered read-only and so has no editor to apply it.
  state.blipNodes.set(blip.id, row);

  if (state.playback || !mode.allowsEdit(blip)) {
    editorRoot.contentEditable = 'false';
    editorRoot.classList.add('read-only');
    renderDelta(editorRoot, doc.doc);
    return row;
  }

  const editor = new Editor(editorRoot, doc.doc, {
    onChange: (delta) => onLocalEdit(blip.id, delta),
    onSelectionChange: (selection) => {
      state.activeEditor = editor;
      dockToolbar();
      // Reported like any other document, so a caret in the margin shows up in
      // the margin. A remark lives in the same wavelet as the page it is about,
      // so this reveals nothing the reader could not already see.
      conn.send({
        type: 'cursor',
        waveId: state.waveId,
        blipId: blip.id,
        index: selection.index,
        length: selection.length,
      });
    },
  });
  editor.doc = doc.doc;
  editor.blipId = blip.id;
  editor.waveletId = blip.waveletId;
  state.editors.set(blip.id, editor);
  return row;
}

/// The reply box at the foot of the active card.
function commentReply(thread) {
  const field = el('div', { class: 'comment-compose-input' });
  const send = () => {
    const draft = state.commentDrafts.get(thread.id);
    if (!draft || !draft.toPlainText().trim()) return;
    conn.send({ type: 'replyToComment', commentId: thread.id, content: draft });
    state.commentDrafts.delete(thread.id);
    editor.reset(new Delta());
  };

  const box = el('div', { class: 'comment-compose' }, [
    field,
    el('button', {
      class: 'btn primary comment-send',
      text: 'Reply',
      onMousedown: (e) => e.preventDefault(),
      onClick: (e) => {
        e.stopPropagation();
        send();
      },
    }),
  ]);

  // Drafts survive a rebuild of the margin, which happens whenever anyone else
  // adds a remark. Losing half a sentence because someone else was typing is
  // exactly the failure the thread's own re-render avoids for the page.
  const draft = state.commentDrafts.get(thread.id) || new Delta();
  const editor = new Editor(field, draft, {
    onChange: () => state.commentDrafts.set(thread.id, editor.doc),
    onSelectionChange: () => {
      state.activeEditor = editor;
      state.commentReplyFocused = true;
      dockToolbar();
    },
  });
  // Enter sends, Shift+Enter is a newline — the same bargain as the chat
  // composer. The editor only calls this when Shift is up.
  editor.onEnter = (event) => {
    event.preventDefault();
    send();
    return false;
  };
  box.replyEditor = editor;
  return box;
}

/**
 * Put each card beside the text it is about, without letting two overlap.
 *
 * Cards are placed top down and pushed past the one above when they would
 * collide, which is what keeps a run of comments on adjacent lines readable
 * while still pointing roughly at the right place.
 */
function layoutComments() {
  const rail = state.commentRail;
  if (!rail || !rail.isConnected) return;
  const cards = Array.from(rail.querySelectorAll('.comment-card'));
  if (cards.length === 0) {
    rail.style.height = '';
    return;
  }

  // Read the anchors afresh rather than trusting what the card was built with.
  // They live in the documents and the documents have moved since — which is
  // the entire reason for putting them there.
  const anchors = anchorsByComment();
  const placed = cards.map((card) => {
    const anchor = anchors.get(card.dataset.comment) || null;
    const rect = anchor ? anchorRect(anchor) : null;
    // A thread whose words have been edited away has nothing to sit beside.
    card.classList.toggle('detached', !anchor);
    return { card, want: rect ? rect.top : Number.POSITIVE_INFINITY };
  });

  // On a narrow screen the margin becomes an ordinary list under the page, and
  // the stylesheet says so by taking the rail out of absolute positioning.
  //
  // Playback stacks them too. The page is showing a rewound document while the
  // anchors are read from the live one, so there is no honest line to put a
  // card beside — better a list in the margin than cards pointing confidently
  // at the wrong words. The class takes the *cards* out of absolute
  // positioning, not the rail, so reading `position` back below still reports
  // what the stylesheet decided about the screen width.
  rail.classList.toggle('stacked', Boolean(state.playback));
  if (state.playback || getComputedStyle(rail).position === 'static') {
    for (const { card } of placed) card.style.top = '';
    rail.style.height = '';
    return;
  }

  // Detached cards have no line to sit beside, so they sink to the bottom.
  placed.sort((a, b) => a.want - b.want);
  const base = rail.getBoundingClientRect().top;
  // Every height read before the first `top` is written. Interleaving them
  // forces the browser to lay the page out again between each pair, on a path
  // that runs on every keystroke.
  for (const entry of placed) entry.height = entry.card.offsetHeight;

  let floor = 0;
  for (const { card, want, height } of placed) {
    const top = Math.max(floor, Number.isFinite(want) ? want - base : floor);
    card.style.top = `${top}px`;
    floor = top + height + 8;
  }
  // Absolutely positioned children do not stretch their parent, and without a
  // height the last cards would be unreachable in a short document.
  rail.style.height = `${floor}px`;
  // A card is a document like any other, so someone else's caret can be inside
  // one. Having just moved the cards, every such caret is drawn where its card
  // used to be.
  if (state.cursorLayer) state.cursorLayer.render();
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

  // Typing in the page moves the words the cards point at, and a re-render of
  // the editor rebuilds the marks from scratch. Both need the margin to catch
  // up; neither is a reason to rebuild the cards themselves.
  markAnchors(allComments());
  layoutComments();
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

  // Only where the server would take one. Offering it in a chat would put a
  // button on the page whose only outcome is a refusal.
  if (mode.allowsComments()) {
    bar.appendChild(el('button', {
      class: 'tool tool-comment',
      type: 'button',
      title: 'Comment on the selected text',
      'aria-label': 'Comment on the selected text',
      onMousedown: (e) => e.preventDefault(),
      onClick: () => startComment(),
    }, [icon(ICONS.comment)]));
  }

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
    placeholder: 'new password',
    description: 'At least 12 characters, and not your username or a password '
      + 'everyone else has already picked.',
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

/**
 * End every other session on this account.
 *
 * Changing your password was the only way to do this, which is a strange thing
 * to have to do about a laptop left on a train — and it gave no idea how many
 * sessions there were to end.
 */
async function signOutEverywhere() {
  let sessions = null;
  try {
    ({ sessions } = await api('/api/sessions'));
  } catch {
    // Fall through: not knowing the count is no reason to refuse the action.
  }
  const others = sessions === null ? null : Math.max(0, sessions - 1);
  if (others === 0) {
    toast('This is your only signed-in session.');
    return;
  }

  const ok = await confirmAction(
    'Sign out everywhere else',
    others === null
      ? 'Every other browser and device signed in to this account will be signed out.'
      : `${others} other ${others === 1 ? 'session' : 'sessions'} will be signed out.`
        + ' This one stays signed in.',
    { confirmLabel: 'Sign out others', danger: true },
  );
  if (!ok) return;

  try {
    const { revoked } = await api('/api/sessions/revoke', { method: 'POST' });
    toast(revoked === 0
      ? 'There was nothing else to sign out.'
      : `Signed out ${revoked} other ${revoked === 1 ? 'session' : 'sessions'}.`);
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
    const empty = allBlips().find((b) => !b.comment && (b.content.ops || []).length === 0);
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
  // Someone else's edit moved the text, so it moved the anchors with it. Only
  // the positions change here — rebuilding the cards would throw away a reply
  // being typed just because a colleague was typing too.
  markAnchors(allComments());
  layoutComments();
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
  // A remark is excluded: the caret belongs in the reply box it was just sent
  // from, and scrolling the page to a card in the margin moves the very text
  // the comment is about out from under the person reading it.
  if (message.blip.author === state.me.id && !state.composer && !message.blip.comment) {
    const editor = state.editors.get(message.blip.id);
    if (editor) {
      editor.root.focus();
      editor.root.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
  }
});

conn.on('commentAdded', (message) => {
  if (message.waveId !== state.waveId || !state.wave) return;
  const wavelet = state.wave.wavelets.find((w) => w.id === message.comment.waveletId);
  if (!wavelet) return;
  wavelet.comments = wavelet.comments || [];
  if (!wavelet.comments.some((c) => c.id === message.comment.id)) {
    wavelet.comments.push(message.comment);
  }
  if (!wavelet.blips.some((b) => b.id === message.blip.id)) wavelet.blips.push(message.blip);

  const mine = message.comment.author === state.me.id;
  if (mine) state.activeComment = message.comment.id;
  renderComments();

  // Our own thread opens with the caret in its first remark, which the server
  // created empty and is waiting to be written. Someone else's just appears.
  if (mine) {
    const editor = state.editors.get(message.blip.id);
    if (editor) editor.root.focus();
  } else {
    restoreCommentFocus();
  }
});

conn.on('commentResolved', (message) => {
  if (message.waveId !== state.waveId || !state.wave) return;
  for (const wavelet of state.wave.wavelets) {
    const thread = (wavelet.comments || []).find((c) => c.id === message.commentId);
    if (!thread) continue;
    thread.resolvedBy = message.resolvedBy;
    thread.resolvedAt = message.resolvedAt;
  }
  // A settled thread leaves the margin, so there is nothing left to be active.
  if (message.resolvedBy && state.activeComment === message.commentId) {
    state.activeComment = null;
    state.commentReplyFocused = false;
  }
  renderComments();
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
  // The open wave holds its own copy of the flags, taken from the snapshot it
  // was opened with, and the header's buttons are drawn from that copy. Without
  // this it never hears about a change it made itself: Archive has always gone
  // on saying "Archive" after archiving, until the wave was closed and reopened.
  if (state.wave && message.summary.id === state.waveId) {
    const before = JSON.stringify(state.wave.flags);
    state.wave.flags = message.summary.flags;
    if (JSON.stringify(state.wave.flags) !== before) renderWave();
  }
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
  // Rewrapping the page moves every line, and the cards sit beside lines.
  layoutComments();
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
