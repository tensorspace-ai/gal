// Connection to the server, and the client half of the OT protocol.

import { Delta, compose, transform, transformPosition } from './ot.js';

/**
 * One blip's document, plus the state machine that lets you keep typing while
 * an earlier edit is still in flight.
 *
 * At any moment a client is in one of three states:
 *
 * - **synchronised** — nothing in flight (`outstanding` is null)
 * - **awaiting ack** — one op sent, not yet confirmed
 * - **awaiting ack with buffer** — one op sent, further edits queued locally
 *
 * Only ever one op is in flight. Queued edits are composed into a single
 * `buffer` op and sent when the outstanding one is acknowledged, which keeps
 * the server's transform history short no matter how fast someone types.
 */
/**
 * A short unique id for one submitted op.
 *
 * `crypto.randomUUID` needs a secure context, which a plain-HTTP deployment on
 * a non-localhost host is not, so fall back rather than throw.
 */
let opCounter = 0;
const opPrefix =
  typeof crypto !== 'undefined' && crypto.randomUUID
    ? crypto.randomUUID().slice(0, 8)
    : Math.random().toString(36).slice(2, 10);

export function newOpId() {
  opCounter += 1;
  return `${opPrefix}-${opCounter}`;
}

export class BlipDoc {
  constructor(blipId, content, revision) {
    this.id = blipId;
    this.doc = new Delta(content);
    this.revision = revision;
    this.outstanding = null;
    this.buffer = null;
    /// Id of the op currently in flight, so a replay after a reconnect is
    /// recognisable by the server as the same op rather than a new edit.
    this.outstandingOpId = null;
  }

  get text() {
    return this.doc.toPlainText();
  }

  /** Is every local edit confirmed by the server? */
  isSynchronised() {
    return this.outstanding === null && this.buffer === null;
  }

  /**
   * Apply a local edit. Returns the op to send now, or null if one is already
   * in flight and this edit was buffered.
   */
  applyLocal(delta) {
    this.doc = compose(this.doc, delta);

    if (this.outstanding === null) {
      this.outstanding = delta;
      this.outstandingOpId = newOpId();
      return delta;
    }
    this.buffer = this.buffer === null ? delta : compose(this.buffer, delta);
    return null;
  }

  /**
   * Apply an op from another participant, rebasing it past anything of ours
   * that the server has not seen yet.
   *
   * Returns the delta to apply to the rendered view — which is *not* the op as
   * received, because our own unconfirmed edits shifted the positions.
   */
  applyRemote(delta, revision) {
    this.revision = revision;

    if (this.outstanding === null) {
      this.doc = compose(this.doc, delta);
      return delta;
    }

    // The server gives priority to what it has already committed, and rebases
    // our op the same way when it arrives, so both sides agree.
    const newOutstanding = transform(delta, this.outstanding, true);
    let rebased = transform(this.outstanding, delta, false);
    let newBuffer = null;

    if (this.buffer !== null) {
      newBuffer = transform(rebased, this.buffer, true);
      rebased = transform(this.buffer, rebased, false);
    }

    this.outstanding = newOutstanding;
    this.buffer = newBuffer;
    this.doc = compose(this.doc, rebased);
    return rebased;
  }

  /**
   * The server confirmed our outstanding op. Returns the next op to send, if
   * edits queued up while we were waiting.
   */
  acknowledge(revision) {
    this.revision = revision;
    if (this.buffer !== null) {
      this.outstanding = this.buffer;
      this.buffer = null;
      this.outstandingOpId = newOpId();
      return this.outstanding;
    }
    this.outstanding = null;
    this.outstandingOpId = null;
    return null;
  }

  /**
   * Everything typed locally that the server has not acknowledged — the op in
   * flight composed with anything queued behind it.
   *
   * After a reconnect the server has no memory of this connection, so the owner
   * re-applies this against the fresh snapshot rather than letting it be lost.
   */
  pendingWork() {
    if (this.outstanding === null) return this.buffer;
    if (this.buffer === null) return this.outstanding;
    return compose(this.outstanding, this.buffer);
  }

  /** Map a local caret position across an incoming remote change. */
  transformCursor(index, delta) {
    return transformPosition(delta, index, true);
  }

  /** Discard local state and adopt the server's, after a failed resync. */
  reset(content, revision) {
    this.doc = new Delta(content);
    this.revision = revision;
    this.outstanding = null;
    this.buffer = null;
  }
}

/**
 * WebSocket connection with automatic reconnection.
 *
 * Listeners are registered per message type. On reconnect every open wave is
 * re-opened from scratch, which resynchronises documents that drifted while the
 * connection was down.
 */
export class Connection {
  constructor() {
    this.socket = null;
    this.listeners = new Map();
    this.openWaves = new Set();
    this.reconnectDelay = 500;
    this.shouldReconnect = true;
    this.queue = [];
    this.status = 'connecting';
  }

  on(type, handler) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(handler);
    return this;
  }

  emit(type, payload) {
    for (const handler of this.listeners.get(type) || []) handler(payload);
  }

  setStatus(status) {
    if (this.status === status) return;
    this.status = status;
    this.emit('status', status);
  }

  connect() {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const socket = new WebSocket(`${protocol}//${location.host}/ws`);
    this.socket = socket;

    socket.addEventListener('open', () => {
      this.setStatus('online');
      this.reconnectDelay = 500;

      // Re-open every wave *first*: a reconnected socket is a new session on
      // the server, with no subscriptions, and it rejects commands that refer
      // to waves it has not been told this connection is watching.
      for (const waveId of this.openWaves) {
        this.send({ type: 'open', waveId });
      }
      const queued = this.queue.splice(0);
      for (const message of queued) this.send(message);
    });

    socket.addEventListener('message', (event) => {
      let message;
      try {
        message = JSON.parse(event.data);
      } catch {
        return;
      }
      this.emit(message.type, message);
      this.emit('*', message);
    });

    socket.addEventListener('close', () => {
      this.setStatus('offline');
      if (!this.shouldReconnect) return;
      // Back off up to 10s so a server restart does not get hammered.
      setTimeout(() => this.connect(), this.reconnectDelay);
      this.reconnectDelay = Math.min(this.reconnectDelay * 2, 10000);
    });

    socket.addEventListener('error', () => socket.close());
  }

  send(message) {
    if (this.socket && this.socket.readyState === WebSocket.OPEN) {
      this.socket.send(JSON.stringify(message));
      return true;
    }
    // Ops are deliberately not queued here. They are written against a specific
    // revision, which a restarted server will not accept, and the document
    // keeps them as unacknowledged work — they are re-applied against the fresh
    // snapshot on resync instead. Cursor hints are dropped outright since a
    // newer one always follows.
    if (message.type !== 'cursor' && message.type !== 'submit') {
      this.queue.push(message);
    }
    return false;
  }

  openWave(waveId) {
    this.track(waveId);
    this.send({ type: 'open', waveId });
  }

  /**
   * Remember that this connection is watching a wave, so it is re-opened after a
   * reconnect. Called separately from `openWave` because the server also opens a
   * wave on the client's behalf when it creates one — and a wave the client
   * never explicitly opened still has to survive a dropped connection.
   */
  track(waveId) {
    this.openWaves.add(waveId);
  }

  closeWave(waveId) {
    this.openWaves.delete(waveId);
    this.send({ type: 'close', waveId });
  }

  disconnect() {
    this.shouldReconnect = false;
    if (this.socket) this.socket.close();
  }
}
