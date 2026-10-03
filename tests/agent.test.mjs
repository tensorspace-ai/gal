import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { test } from 'node:test';
import { GalAgent, GalAgentError } from '../tools/agent-client.mjs';

const token = 'gal_agent_test_secret';
async function serve(t, handle) {
  const server = http.createServer((req, res) => {
    Promise.resolve(handle(req, res)).catch(error => { res.destroy(error); });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(async () => {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  });
  const baseUrl = `http://127.0.0.1:${server.address().port}`;
  return { baseUrl, client: new GalAgent({ baseUrl, token }) };
}
function json(res, status, value, headers = {}) {
  res.writeHead(status, { 'Content-Type': 'application/json', ...headers });
  res.end(JSON.stringify(value));
}
async function body(req) {
  let text = '';
  for await (const chunk of req) text += chunk;
  return text;
}
function cli(baseUrl, args, stdin = '') {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['tools/agent.mjs', ...args], {
      env: { ...process.env, GAL_URL: baseUrl, GAL_AGENT_TOKEN: token }, stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '', stderr = '';
    child.stdout.on('data', data => { stdout += data; });
    child.stderr.on('data', data => { stderr += data; });
    child.on('error', reject);
    child.on('close', code => resolve({ code, stdout, stderr }));
    child.stdin.end(stdin);
  });
}

test('context follows bounded pages and preserves source ids, revisions and Unicode', async t => {
  const { client } = await serve(t, (req, res) => {
    assert.equal(req.headers.authorization, `Bearer ${token}`);
    const url = new URL(req.url, 'http://local');
    assert.equal(url.searchParams.get('limit'), '1');
    assert.equal(url.searchParams.get('textUnits'), '8');
    const after = url.searchParams.get('after');
    json(res, 200, { waveletId: 's-test', blips: [{ id: after ? 'b-two' : 'b-one', revision: 4, text: '😀' }], nextCursor: after ? null : '0:b-one' });
  });
  const pages = [];
  for await (const page of client.pages({ limit: 1, textUnits: 8 })) pages.push(page);
  assert.deepEqual(pages.map(p => p.blips[0].id), ['b-one', 'b-two']);
  assert.equal(pages[0].blips[0].revision, 4);
  assert.equal(pages[0].blips[0].text, '😀');
});

test('uncertain replies reuse exactly one body and request id across network and HTTP retries', async t => {
  const sent = [];
  const { client } = await serve(t, async (req, res) => {
    assert.equal(req.method, 'POST');
    assert.equal(req.url, '/api/agent/replies');
    sent.push(await body(req));
    if (sent.length === 1) { req.socket.destroy(); return; }
    if (sent.length === 2) { json(res, 503, { error: 'temporarily unavailable' }, { 'Retry-After': '0' }); return; }
    json(res, 200, { requestId: JSON.parse(sent[0]).requestId, blipId: 'b-once', revision: 1 });
  });
  const receipt = await client.reply({ parent: 'b-parent', text: 'Research 😀' });
  assert.equal(sent.length, 3);
  assert.equal(new Set(sent).size, 1);
  assert.equal(receipt.requestId, JSON.parse(sent[0]).requestId);
  assert.equal(receipt.blipId, 'b-once');
});

test('authorization refusals and request conflicts are never automatically retried', async t => {
  for (const status of [401, 403, 409]) {
    let calls = 0;
    const { client } = await serve(t, (req, res) => { calls++; json(res, status, { error: `refused ${token}`, code: 'refused' }); });
    await assert.rejects(client.reply({ text: 'draft', requestId: 'stable' }), error => {
      assert.ok(error instanceof GalAgentError);
      assert.equal(error.status, status);
      assert.equal(error.requestId, 'stable');
      assert.equal(error.message.includes(token), false);
      return true;
    });
    assert.equal(calls, 1);
  }
});

test('rate limits use Retry-After and cancellation interrupts a retry wait', async t => {
  let calls = 0;
  const { client } = await serve(t, (req, res) => {
    calls++;
    json(res, 429, { error: 'slow down' }, { 'Retry-After': '5' });
  });
  const controller = new AbortController();
  const pending = client.context({ signal: controller.signal });
  while (calls === 0) await new Promise(resolve => setTimeout(resolve, 5));
  // Let the first response arrive so this covers cancellation during backoff too.
  await new Promise(resolve => setTimeout(resolve, 30));
  controller.abort();
  await assert.rejects(pending, { name: 'AbortError' });
  assert.equal(calls, 1);
});

test('timeouts report an uncertain outcome and keep its request id', async t => {
  const { baseUrl } = await serve(t, () => {});
  const client = new GalAgent({ baseUrl, token, timeoutMs: 30, attempts: 1 });
  await assert.rejects(client.reply({ text: 'draft', requestId: 'resume-this' }), error => {
    assert.equal(error.requestId, 'resume-this');
    assert.equal(error.code, 'network');
    return true;
  });
});

test('redirects cannot forward a credential to another endpoint', async t => {
  let forwarded = 0;
  const destination = await serve(t, (req, res) => { forwarded++; json(res, 200, {}); });
  const source = await serve(t, (req, res) => { res.writeHead(302, { Location: destination.baseUrl }); res.end(); });
  const client = new GalAgent({ baseUrl: source.baseUrl, token, attempts: 1 });
  await assert.rejects(client.context(), GalAgentError);
  assert.equal(forwarded, 0);
});

test('CLI consumes stdin, emits JSON receipts, and rejects a reply without a persisted key', async t => {
  const requests = [];
  const { baseUrl } = await serve(t, async (req, res) => {
    requests.push(JSON.parse(await body(req)));
    json(res, 201, { requestId: requests[0].requestId, blipId: 'b-cli', revision: 1 });
  });
  const result = await cli(baseUrl, ['reply', '--request-id', 'cli-run', '--parent', 'b-parent'], 'Answer 😀\n');
  assert.equal(result.code, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).blipId, 'b-cli');
  assert.deepEqual(requests[0], { requestId: 'cli-run', parent: 'b-parent', text: 'Answer 😀\n' });
  assert.equal(result.stdout.includes(token), false);
  const invalid = await cli(baseUrl, ['reply'], 'answer');
  assert.equal(invalid.code, 1);
  assert.equal(requests.length, 1);
});

test('CLI returns structured HTTP errors without printing credentials', async t => {
  const { baseUrl } = await serve(t, (req, res) => json(res, 403, { error: `forbidden ${token}`, code: 'forbidden' }));
  const result = await cli(baseUrl, ['context', '--limit', '1']);
  assert.equal(result.code, 1);
  assert.equal(JSON.parse(result.stderr).code, 'forbidden');
  assert.equal(result.stderr.includes(token), false);
});

test('long Retry-After values return to the scheduler without an early retry', async t => {
  let calls = 0;
  const { client } = await serve(t, (req, res) => {
    calls++;
    json(res, 429, { error: 'retry later' }, { 'Retry-After': '120' });
  });
  await assert.rejects(client.reply({ requestId: 'scheduled-job', text: 'answer' }), error => {
    assert.equal(error.retryAfterMs, 120000);
    assert.equal(error.status, 429);
    assert.equal(error.requestId, 'scheduled-job');
    return true;
  });
  assert.equal(calls, 1);
});

test('a cancelled reply retains its generated request id for recovering an uncertain post', async t => {
  let sent;
  const { client } = await serve(t, async (req) => { sent = JSON.parse(await body(req)); });
  const controller = new AbortController();
  const pending = client.reply({ text: 'may have landed', signal: controller.signal });
  while (!sent) await new Promise(resolve => setTimeout(resolve, 5));
  controller.abort();
  await assert.rejects(pending, error => {
    assert.ok(error instanceof GalAgentError);
    assert.equal(error.name, 'AbortError');
    assert.equal(error.code, 'cancelled');
    assert.equal(error.requestId, sent.requestId);
    return true;
  });
});
