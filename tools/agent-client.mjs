import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';

export class GalAgentError extends Error {
  constructor(message, { status = 0, code = 'network', requestId, retryAfterMs } = {}) {
    super(message);
    this.name = 'GalAgentError';
    this.status = status;
    this.code = code;
    this.requestId = requestId;
    this.retryAfterMs = retryAfterMs;
  }
}

/** Headless reads and posts; no browser DOM or OT state machine is required. */
export class GalAgent {
  #token;
  #base;
  #timeout;
  #attempts;

  constructor({ baseUrl = 'http://127.0.0.1:8080', token, timeoutMs = 30000, attempts = 3 } = {}) {
    const base = new URL(baseUrl);
    if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password || base.search || base.hash) {
      throw new TypeError('baseUrl must be an HTTP(S) URL without credentials, a query or a fragment.');
    }
    if (typeof token !== 'string' || !token || /\s/.test(token)) throw new TypeError('A bearer token is required.');
    if (!Number.isInteger(attempts) || attempts < 1 || attempts > 5) throw new TypeError('attempts must be 1–5.');
    if (!Number.isInteger(timeoutMs) || timeoutMs < 1) throw new TypeError('timeoutMs must be a positive integer.');
    this.#base = base.toString().replace(/\/$/, '');
    this.#token = token;
    this.#timeout = timeoutMs;
    this.#attempts = attempts;
  }

  context({ after, limit = 50, textUnits = 16000, signal } = {}) {
    if (!Number.isInteger(limit) || limit < 1 || limit > 100 || !Number.isInteger(textUnits) || textUnits < 2 || textUnits > 64000) throw new TypeError('Invalid context bounds.');
    const query = new URLSearchParams({ limit: String(limit), textUnits: String(textUnits) });
    if (after != null) query.set('after', after);
    return this.#request(`/api/agent/context?${query}`, { signal });
  }

  /** Each page is a fresh read; pagination does not replay edits or recover truncated text. */
  async *pages({ after, signal, ...options } = {}) {
    let cursor = after;
    do {
      const page = await this.context({ ...options, after: cursor, signal });
      yield page;
      if (page.nextCursor != null && page.nextCursor === cursor) throw new GalAgentError('Context cursor did not advance.', { code: 'invalidResponse' });
      cursor = page.nextCursor;
    } while (cursor != null);
  }

  /** Persist requestId before calling if a process restart must retry the same post. */
  reply({ text, parent = null, requestId = randomUUID(), signal } = {}) {
    if (typeof text !== 'string' || !text || text.length > 262144) throw new TypeError('Text must contain 1–262144 UTF-16 units.');
    if (parent !== null && (typeof parent !== 'string' || !/^b-[A-Za-z0-9]{1,62}$/.test(parent))) throw new TypeError('Invalid parent id.');
    if (typeof requestId !== 'string' || !/^[A-Za-z0-9_-]{1,100}$/.test(requestId)) throw new TypeError('Invalid requestId.');
    return this.#request('/api/agent/replies', {
      method: 'POST', body: { requestId, parent, text }, signal, requestId,
    });
  }

  async #request(path, { method = 'GET', body, signal, requestId } = {}) {
    // Serialize once and reuse the identical body and key for every retry.
    const payload = body === undefined ? undefined : JSON.stringify(body);
    for (let attempt = 0; attempt < this.#attempts; attempt++) {
      if (signal?.aborted) throw cancellation(requestId);
      const controller = new AbortController();
      const abort = () => controller.abort(signal.reason);
      signal?.addEventListener('abort', abort, { once: true });
      const timer = setTimeout(() => controller.abort(new Error('Request timed out.')), this.#timeout);
      let waitMs = Math.min(500 * 2 ** attempt, 5000);
      let failure;
      try {
        const response = await fetch(this.#base + path, {
          method, headers: { Authorization: `Bearer ${this.#token}`, Accept: 'application/json',
            ...(payload === undefined ? {} : { 'Content-Type': 'application/json' }) },
          body: payload, signal: controller.signal, redirect: 'error',
        });
        const text = await response.text();
        let value;
        try { value = JSON.parse(text); } catch { /* Fall back to a status-based error without response content. */ }
        if (response.ok && value && typeof value === 'object') return value;
        const message = typeof value?.error === 'string'
          ? value.error.replaceAll(this.#token, '[redacted]') : `Gal returned HTTP ${response.status}.`;
        failure = new GalAgentError(message, { status: response.status, code: value?.code ?? 'http', requestId });
        if (response.ok) throw new GalAgentError('Gal returned an invalid JSON response.', { status: response.status, code: 'invalidResponse', requestId });
        if (![429, 500, 502, 503, 504].includes(response.status)) throw failure;
        const retryAfter = response.headers.get('retry-after');
        if (retryAfter != null) {
          const seconds = Number(retryAfter);
          const ms = Number.isFinite(seconds) ? seconds * 1000 : Date.parse(retryAfter) - Date.now();
          if (Number.isFinite(ms) && ms >= 0) {
            failure.retryAfterMs = ms;
            // Long waits belong to the caller's job scheduler, rather than an
            // early retry that ignores the server's allowance.
            if (ms > 60000) throw failure;
            waitMs = ms;
          }
        }
      } catch (error) {
        if (signal?.aborted) throw cancellation(requestId);
        if (error instanceof GalAgentError) throw error;
        // Network failures and per-attempt timeouts have an uncertain outcome.
        failure = new GalAgentError('Could not reach Gal or the request timed out.', { requestId });
      } finally {
        clearTimeout(timer);
        signal?.removeEventListener('abort', abort);
      }
      if (attempt + 1 === this.#attempts) throw failure;
      try {
        await delay(waitMs, undefined, { signal });
      } catch (error) {
        if (signal?.aborted) throw cancellation(requestId);
        throw error;
      }
    }
  }
}

function cancellation(requestId) {
  const error = new GalAgentError('Request cancelled.', { code: 'cancelled', requestId });
  error.name = 'AbortError';
  return error;
}
