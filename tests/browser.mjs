// Browser tests: drive the real UI in real browser sessions.
//
//   npm install --prefix tests
//   node tests/browser.mjs
//
// The Rust tests prove the server and the protocol. These prove the parts only a
// browser can exercise: the contenteditable editor, caret handling, rendering,
// the responsive layout, and what actually happens to your typing when the
// connection dies mid-word.
//
// The server is started and stopped by this script, against a throwaway
// database, so killing it mid-test is a legitimate thing to do.

import { chromium } from 'playwright-core';
import { spawn, execSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.GAL_TEST_PORT || 8117);
const BASE = `http://127.0.0.1:${PORT}`;
const PASSWORD = 'correct horse battery';

let failures = 0;
let checks = 0;

function check(name, ok, detail) {
  checks += 1;
  console.log(`${ok ? '  ok  ' : ' FAIL '} ${name}${detail ? ` — ${detail}` : ''}`);
  if (!ok) failures += 1;
}

function section(title) {
  console.log(`\n${title}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --- server lifecycle ---------------------------------------------------

const dataDir = mkdtempSync(join(tmpdir(), 'gal-browser-'));
const binary = join(ROOT, 'target/release/gal-server');

try {
  execSync(`test -x ${binary}`);
} catch {
  console.error(`Build the server first:\n  cargo build --release -p gal-server`);
  process.exit(1);
}

let server = null;
function startServer() {
  server = spawn(binary, [], {
    env: { ...process.env, GAL_PORT: String(PORT), GAL_DB: join(dataDir, 'test.db') },
    stdio: 'ignore',
  });
}
function stopServer() {
  if (server) server.kill('SIGKILL');
  server = null;
}

/**
 * Locate a cached Playwright browser, since we depend only on playwright-core
 * (which downloads nothing itself).
 *
 * Returns undefined when nothing is cached, letting Playwright resolve a browser
 * it installed itself — which is what `npx playwright install chromium` provides
 * and what CI relies on. Set GAL_CHROMIUM to point at a specific binary.
 */
function browserPath() {
  if (process.env.GAL_CHROMIUM) return process.env.GAL_CHROMIUM;

  const home = process.env.HOME || process.env.USERPROFILE || '';
  const caches = [
    join(home, 'Library/Caches/ms-playwright'), // macOS
    join(home, '.cache/ms-playwright'), // Linux
    join(process.env.LOCALAPPDATA || '', 'ms-playwright'), // Windows
  ].filter((c) => c && existsSync(c));

  for (const cache of caches) {
    // Search rather than assume an architecture directory, so this works on
    // Intel and Apple Silicon alike.
    for (const dir of readdirSync(cache).filter((d) => d.startsWith('chromium')).sort().reverse()) {
      for (const candidate of [
        'chrome-headless-shell-mac-arm64/chrome-headless-shell',
        'chrome-headless-shell-mac-x64/chrome-headless-shell',
        'chrome-headless-shell-linux64/chrome-headless-shell',
        'chrome-linux/chrome',
        'chrome-mac/Chromium.app/Contents/MacOS/Chromium',
      ]) {
        const full = join(cache, dir, candidate);
        if (existsSync(full)) return full;
      }
    }
  }
  return undefined;
}

startServer();
await sleep(1500);

const browser = await chromium.launch({ executablePath: browserPath() });
const pages = [];

async function signUp(name, viewport = { width: 1280, height: 860 }) {
  const context = await browser.newContext({ viewport });
  const page = await context.newPage();
  pages.push(page);
  page.on('pageerror', (e) => {
    console.log(`  [${name}] uncaught: ${e.message}`);
    failures += 1;
  });
  await page.goto(BASE);
  await page.waitForSelector('.auth-form');
  await page.fill('input[placeholder="username"]', name);
  await page.fill('input[placeholder="display name (optional)"]', name[0].toUpperCase() + name.slice(1));
  await page.fill('input[type="password"]', PASSWORD);
  await page.click('button[type="submit"]');
  await page.waitForSelector('.layout', { timeout: 15000 });
  return page;
}

async function signIn(name) {
  const page = await (await browser.newContext()).newPage();
  pages.push(page);
  await page.goto(BASE);
  await page.waitForSelector('.auth-form');
  await page.click('.btn.link'); // switch to the sign-in form
  await page.fill('input[placeholder="username"]', name);
  await page.fill('input[type="password"]', PASSWORD);
  await page.click('button[type="submit"]');
  await page.waitForSelector('.layout', { timeout: 15000 });
  return page;
}

async function createWave(page, title) {
  await page.click('.new-wave');
  await page.waitForSelector('.dialog input');
  await page.fill('.dialog input', title);
  await page.click('.dialog .btn.primary');
  await page.waitForSelector('.blip .editor', { timeout: 15000 });
}

// isEditable() throws when an element is not editable at all, which is exactly
// the state some modes should produce. Read the attribute instead.
const editable = async (page, selector) =>
  (await page.getAttribute(selector, 'contenteditable')) === 'true';

async function setMode(page, id) {
  await page.selectOption('.mode-select', id);
  await page.waitForSelector('.dialog .btn.primary', { timeout: 5000 });
  await page.click('.dialog .btn.primary');
  await page.waitForTimeout(900);
}

async function addParticipant(page, name) {
  await page.click('.add-participant');
  await page.waitForSelector('.dialog input');
  await page.fill('.dialog input', name);
  await page.click('.dialog .btn.primary');
  await page.waitForTimeout(900);
}

try {
  // --- accounts and waves ---------------------------------------------

  section('Accounts and waves');
  const alice = await signUp('alice');
  const bob = await signUp('bob');
  check('a new account reaches the app shell', await alice.isVisible('.sidebar'));

  await createWave(alice, 'Launch plan');
  check('creating a wave opens it, ready to type', await alice.isVisible('.editor'));
  check('the title is shown', (await alice.textContent('.wave-title')) === 'Launch plan');

  await alice.click('.editor');
  await alice.keyboard.type('We ship on Friday.');
  await alice.waitForTimeout(400);
  check(
    'typing appears in the editor',
    (await alice.textContent('.editor')).includes('We ship on Friday.'),
  );

  // --- live delivery ---------------------------------------------------

  section('Live delivery');
  await addParticipant(alice, 'bob');
  check('the participant list grows', (await alice.locator('.participants .avatar').count()) === 2);

  await bob.waitForSelector('.inbox-row', { timeout: 15000 });
  check('the wave reaches the other inbox without a reload',
    (await bob.textContent('.inbox-row')).includes('Launch plan'));
  check('it arrives marked unread', await bob.isVisible('.inbox-row.unread'));

  await bob.click('.inbox-row');
  await bob.waitForSelector('.blip .editor', { timeout: 15000 });
  await bob.waitForTimeout(400);
  check('the existing text is delivered',
    (await bob.textContent('.editor')).includes('We ship on Friday.'));
  check('presence shows the other participant',
    (await alice.locator('.presence-dot').count()) >= 1);

  // --- concurrent editing ----------------------------------------------

  section('Concurrent editing');
  // Both edit the same message at once, in different places, neither waiting.
  await bob.click('.editor');
  await bob.keyboard.press('End');
  await bob.keyboard.type(' Bring donuts.');
  await alice.click('.editor');
  await alice.keyboard.press('Home');
  await alice.keyboard.type('Heads up: ');

  await alice.waitForTimeout(1400);
  await bob.waitForTimeout(500);

  const aliceText = (await alice.textContent('.editor')).trim();
  const bobText = (await bob.textContent('.editor')).trim();
  check('both browsers converge on identical text', aliceText === bobText,
    `alice="${aliceText}" bob="${bobText}"`);
  check('neither edit was lost',
    aliceText.includes('Heads up:') && aliceText.includes('Bring donuts'), aliceText);

  // --- rich text --------------------------------------------------------

  section('Rich text');
  await alice.click('.editor');
  await alice.keyboard.press('End');
  await alice.keyboard.type(' emphasis');
  for (let i = 0; i < 8; i += 1) await alice.keyboard.press('Shift+ArrowLeft');
  await alice.click('.tool-bold');
  await alice.waitForTimeout(600);
  check('bold produces real markup', (await alice.locator('.editor strong').count()) > 0);
  await bob.waitForTimeout(700);
  check('formatting replicates', (await bob.locator('.editor strong').count()) > 0);
  check('the controls are with the message being written, not in the wave header',
    (await alice.locator('.blip .blip-tools .toolbar').count()) === 1 &&
      (await alice.locator('.wave-head .toolbar').count()) === 0);

  // --- threading --------------------------------------------------------

  section('Threading');
  await alice.hover('.blip');
  await alice.click('.blip .blip-action:has-text("Reply")');
  await alice.waitForTimeout(1000);
  check('a reply nests under its parent',
    (await alice.locator('.blip-children .blip').count()) === 1);
  await alice.keyboard.type('Replying in a thread.');
  await alice.waitForTimeout(800);
  await bob.waitForTimeout(600);
  check('the reply is delivered live',
    (await bob.textContent('.blip-children .editor')).includes('Replying in a thread'));

  // Bob is mid-sentence when Alice adds a message. Every node in the thread is
  // rebuilt underneath him; the caret has to survive that, or a busy wave is
  // unwritable.
  await bob.click('.blip .editor');
  await bob.keyboard.press('End');
  await bob.keyboard.type(' —bob');
  await bob.waitForTimeout(300);
  await alice.click('.thread-foot .btn.primary');
  await alice.waitForTimeout(1200);
  await bob.keyboard.type('!');
  await bob.waitForTimeout(500);
  check('a message arriving does not throw you out of the one you are writing',
    (await bob.locator('.blip .editor').first().textContent()).includes('—bob!'));

  // --- private replies --------------------------------------------------

  section('Private replies');
  const carol = await signUp('carol');
  await addParticipant(alice, 'carol');

  await alice.hover('.blip');
  await alice.click('.blip .blip-action:has-text("Privately")');
  await alice.waitForSelector('.dialog input');
  await alice.fill('.dialog input', 'bob');
  await alice.click('.dialog .btn.primary');
  await alice.waitForTimeout(1100);
  check('the private thread appears for its author', await alice.isVisible('.private-thread'));

  await alice.click('.private-thread .editor');
  await alice.keyboard.type('Between us: the date may slip.');
  await alice.waitForTimeout(900);
  await bob.waitForTimeout(900);
  check('an included participant sees it', await bob.isVisible('.private-thread'));

  await carol.waitForSelector('.inbox-row', { timeout: 15000 });
  await carol.click('.inbox-row');
  await carol.waitForSelector('.blip .editor', { timeout: 15000 });
  await carol.waitForTimeout(900);
  check('an excluded participant does not see the thread',
    !(await carol.isVisible('.private-thread')));
  check('nor its contents anywhere in the wave',
    !(await carol.textContent('.thread')).includes('date may slip'));
  check('nor in their inbox snippet',
    !(await carol.textContent('.inbox-row')).includes('date may slip'));

  // --- search -----------------------------------------------------------

  section('Search');
  await alice.fill('.search-input', 'donuts');
  await alice.waitForTimeout(1000);
  check('a match is found', (await alice.locator('.inbox-row').count()) >= 1);
  check('the term is highlighted', (await alice.locator('.inbox-snippet mark').count()) >= 1);

  // A blip whose text is markup must not become markup in anyone's page.
  await alice.fill('.search-input', '');
  await alice.waitForTimeout(300);
  await alice.click('.editor');
  await alice.keyboard.press('End');
  await alice.keyboard.type(' pineapple <img src=x onerror="window.__XSS=1">');
  await alice.waitForTimeout(900);
  await alice.fill('.search-input', 'pineapple');
  await alice.waitForTimeout(1000);
  check('search snippets cannot inject markup',
    (await alice.locator('.inbox-snippet img').count()) === 0);
  check('and no script runs from them',
    !(await alice.evaluate(() => window.__XSS === 1)));
  await alice.fill('.search-input', '');
  await alice.waitForTimeout(400);

  // --- playback ---------------------------------------------------------

  section('Playback');
  await alice.click('.btn.ghost:has-text("Playback")');
  await alice.waitForSelector('.playback-bar', { timeout: 15000 });
  const frames = Number(await alice.getAttribute('.playback-slider', 'max'));
  check('the edit history loads', frames > 5, `${frames} frames`);

  const present = await alice.locator('.blip .editor').first().textContent();
  await alice.locator('.playback-slider').fill('3');
  await alice.dispatchEvent('.playback-slider', 'input');
  await alice.waitForTimeout(500);
  const early = await alice.locator('.blip .editor').first().textContent();
  check('scrubbing back shows an earlier state',
    early.length < present.length && !early.includes('donuts'), `"${early}"`);

  await alice.locator('.playback-slider').fill(String(frames));
  await alice.dispatchEvent('.playback-slider', 'input');
  await alice.waitForTimeout(500);
  check('scrubbing forward restores the present',
    (await alice.locator('.blip .editor').first().textContent()) === present);
  await alice.click('.btn.ghost:has-text("Exit playback")');
  await alice.waitForTimeout(600);

  // --- persistence ------------------------------------------------------

  section('Persistence');
  await bob.reload();
  await bob.waitForSelector('.blip .editor', { timeout: 15000 });
  await bob.waitForTimeout(900);
  check('a full page reload restores the wave',
    (await bob.textContent('.editor')).includes('Heads up:'));

  // --- responsive layout ------------------------------------------------

  section('Narrow screens');
  const phone = await signUp('dave', { width: 390, height: 800 });
  check('a phone starts on the inbox', await phone.isVisible('.sidebar'));
  await createWave(phone, 'Phone wave');
  check('opening a wave gives it the whole screen', !(await phone.isVisible('.sidebar')));
  check('a way back is offered', await phone.isVisible('.back-to-inbox'));
  await phone.click('.back-to-inbox');
  await phone.waitForTimeout(300);
  check('and it returns to the inbox', await phone.isVisible('.sidebar'));


  // --- wave modes -------------------------------------------------------

  section('Wave modes');
  const mAlice = await signUp('mode_alice');
  const mBob = await signUp('mode_bob');
  await createWave(mAlice, 'Modes');
  await mAlice.click('.editor');
  await mAlice.keyboard.type('First message');
  await mAlice.waitForTimeout(400);
  await addParticipant(mAlice, 'mode_bob');
  await mBob.waitForSelector('.inbox-row', { timeout: 15000 });
  await mBob.click('.inbox-row:has-text("Modes")');
  await mBob.waitForSelector('.blip .editor', { timeout: 15000 });
  await mBob.waitForTimeout(500);

  check('the creator gets a mode picker', await mAlice.isVisible('.mode-select'));
  check('everyone else sees a read-only badge', await mBob.isVisible('.mode-badge'));
  check('document mode lets anyone edit anything', await editable(mBob, '.blip .editor'));

  await setMode(mAlice, 'chat');
  await mBob.waitForTimeout(900);
  check('a mode change reaches other participants live',
    (await mBob.textContent('.mode-badge')) === 'Chat');
  check('chat offers a composer', await mAlice.isVisible('.composer-input'));
  check("chat protects other people's messages", !(await editable(mBob, '.blip .editor')));
  check('chat hides threading', !(await mAlice.isVisible('.blip-action:has-text("Reply")')));

  await mAlice.click('.composer-input');
  await mAlice.keyboard.type('sent with enter');
  await mAlice.keyboard.press('Enter');
  await mAlice.waitForTimeout(900);
  check('Enter sends', (await mAlice.locator('.blip').count()) === 2);
  check('the composer clears', (await mAlice.textContent('.composer-input')).trim() === '');
  check('no stray newline is inserted',
    !(await mAlice.locator('.blip .editor').last().textContent()).includes('\n'));
  check('the composer carries its own controls',
    (await mAlice.locator('.composer .toolbar').count()) === 1);

  // Deliberately no click first: after Enter the caret must still be in the
  // composer. It used to be pulled into the message that had just been sent, so
  // the next thing typed silently edited a message already delivered.
  await mAlice.keyboard.type('draft kept');
  await mAlice.waitForTimeout(400);
  check('typing after Enter stays in the composer',
    (await mAlice.textContent('.composer-input')).includes('draft kept'));
  check('and does not edit the message just sent',
    (await mAlice.locator('.blip .editor').last().textContent()).trim() === 'sent with enter');
  await mBob.click('.composer-input');
  await mBob.keyboard.type('from bob');
  await mBob.keyboard.press('Enter');
  await mAlice.waitForTimeout(1200);
  check('an incoming message does not destroy your draft',
    (await mAlice.textContent('.composer-input')).includes('draft kept'));

  const authors = await mAlice.locator('.blip-author').allTextContents();
  check('messages show real author names, including your own',
    authors.includes('Mode_alice') || authors.includes('mode_alice'), authors.join(', '));
  check('no message is attributed to Unknown', !authors.some((a) => a === 'Unknown'));

  // A channel is not a stack of cards. Three messages are on screen here:
  // two from Alice in a row, then one from Bob.
  check('chat lays its messages out as a channel',
    (await mAlice.locator('.blip.chat').count()) === 3);
  check('the day is marked off once', (await mAlice.locator('.day-sep').count()) === 1);
  check('a run from one author shares a header',
    (await mAlice.locator('.blip.chat.grouped').count()) === 1);
  check('and does not repeat the avatar',
    (await mAlice.locator('.blip.chat.grouped .avatar').count()) === 0);
  check('the composer stays out of the scroller, so it cannot scroll away',
    (await mAlice.locator('.thread-scroll .composer').count()) === 0 &&
      (await mAlice.locator('.thread-foot .composer').count()) === 1);
  // Bob's message landed while Alice was watching the bottom of the channel.
  check('a message read as it arrives is not left marked new',
    (await mAlice.locator('.blip.unread').count()) === 0);
  check('nor counted in the inbox of the room you are sitting in',
    (await mAlice.locator('.inbox-row .badge').count()) === 0);

  await setMode(mAlice, 'frozen');
  await mBob.waitForTimeout(900);
  check('frozen makes everything read-only', !(await editable(mAlice, '.blip .editor')));
  check('frozen offers no composer', !(await mAlice.isVisible('.composer-input')));
  check('frozen explains itself', (await mAlice.textContent('.compose-note')).includes('frozen'));

  await setMode(mAlice, 'document');
  await mBob.waitForTimeout(900);
  check('unfreezing restores editing', await editable(mAlice, '.blip .editor'));
  check('threading returns', await mAlice.isVisible('.blip-action:has-text("Reply")'));
  const restored = await mAlice.textContent('.thread');
  check('a mode round trip loses nothing',
    restored.includes('First message') && restored.includes('from bob'), restored.slice(0, 90));

  await setMode(mAlice, 'announcement');
  await mBob.waitForTimeout(900);
  check('announcement stops others posting', await mBob.isVisible('.compose-note'));
  check('but the creator can post', await mAlice.isVisible('.thread-foot .btn'));
  check('and anyone may reply', await mBob.isVisible('.blip-action:has-text("Reply")'));

  await setMode(mAlice, 'notepad');
  await mAlice.waitForTimeout(500);
  check('notepad stays editable by everyone', await editable(mAlice, '.blip .editor'));
  check('notepad adds no new messages', !(await mAlice.isVisible('.thread-foot .btn')));

  // --- attachments ------------------------------------------------------

  section('Attachments');
  // A real 4×4 PNG. The server identifies images by their magic bytes, so a
  // fabricated file would be served back as a download and prove nothing.
  const PNG = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEUlEQVR4nGP8z4AAT' +
      'AxDkw0AV/oCAxTgLGcAAAAASUVORK5CYII=',
    'base64',
  );
  writeFileSync(join(dataDir, 'diagram.png'), PNG);
  writeFileSync(join(dataDir, 'notes.txt'), 'the migration plan, in full\n');
  // HTML wearing a .png extension. Believing the name is how an upload
  // endpoint becomes a way to serve script from your own origin.
  writeFileSync(join(dataDir, 'trojan.png'), '<script>window.__pwned = 1;</script>');

  const fAlice = await signUp('files_alice');
  const fBob = await signUp('files_bob');
  const outsider = await signUp('files_nemo');
  await createWave(fAlice, 'With files');
  await addParticipant(fAlice, 'files_bob');
  await fBob.waitForSelector('.inbox-row', { timeout: 15000 });
  await fBob.click('.inbox-row:has-text("With files")');
  await fBob.waitForSelector('.blip .editor', { timeout: 15000 });

  await fAlice.click('.editor');
  await fAlice.keyboard.type('Here it is:');
  await fAlice.setInputFiles('.file-picker', join(dataDir, 'diagram.png'));
  await fAlice.waitForSelector('.embed-image img', { timeout: 15000 });
  const drawn = await fAlice.evaluate(() => {
    const img = document.querySelector('.embed-image img');
    return img && img.complete && img.naturalWidth > 0;
  });
  check('an image attachment is rendered, and really loads', drawn);

  await fBob.waitForTimeout(1200);
  check('it reaches the other participant live',
    (await fBob.locator('.embed-image img').count()) === 1);

  // The embed is one character of the document. If the DOM walkers disagreed
  // with the model about that, this typing would splice text at the wrong
  // offset and the picture would vanish.
  await fAlice.click('.editor');
  await fAlice.keyboard.press('End');
  await fAlice.keyboard.type(' — signed off');
  await fAlice.waitForTimeout(700);
  check('typing around an embed leaves it alone',
    (await fAlice.locator('.embed-image img').count()) === 1 &&
      (await fAlice.textContent('.editor')).includes('signed off'));
  await fBob.waitForTimeout(900);
  check('and the other side agrees',
    (await fBob.locator('.embed-image img').count()) === 1 &&
      (await fBob.textContent('.editor')).includes('signed off'));

  await fAlice.setInputFiles('.file-picker', join(dataDir, 'notes.txt'));
  await fAlice.waitForSelector('.embed-file', { timeout: 15000 });
  check('a file that is not an image is listed rather than drawn',
    (await fAlice.textContent('.embed-file')).includes('notes.txt'));

  await fAlice.setInputFiles('.file-picker', join(dataDir, 'trojan.png'));
  await fAlice.waitForTimeout(1500);
  check('a name is not evidence: HTML called .png is not treated as an image',
    (await fAlice.locator('.embed-file').count()) === 2 &&
      (await fAlice.locator('.embed-image').count()) === 1);
  const served = await fAlice.evaluate(async () => {
    const links = document.querySelectorAll('.embed-file a');
    const response = await fetch(links[links.length - 1].getAttribute('href'));
    return {
      type: response.headers.get('content-type'),
      disposition: response.headers.get('content-disposition') || '',
    };
  });
  check('and is served as an opaque download',
    served.type === 'application/octet-stream' && served.disposition.startsWith('attachment;'),
    `${served.type} / ${served.disposition}`);

  // PNG magic bytes and then nothing a decoder will take. The server is right
  // to call it an image — that is all its bytes say — and the client has to
  // cope with the picture never arriving.
  writeFileSync(join(dataDir, 'truncated.png'), Buffer.concat([PNG.subarray(0, 8), Buffer.alloc(64)]));
  await fAlice.setInputFiles('.file-picker', join(dataDir, 'truncated.png'));
  await fAlice.waitForTimeout(1500);
  check('an image that will not decode becomes a download rather than a broken icon',
    (await fAlice.locator('.embed-file').count()) === 3 &&
      (await fAlice.textContent('.blip .editor')).includes('truncated.png'));

  const imageHref = await fAlice.locator('.embed-image a').getAttribute('href');
  const outsiderStatus = await outsider.evaluate(async (href) => {
    const response = await fetch(href);
    return response.status;
  }, imageHref);
  check('someone outside the wave cannot fetch the file', outsiderStatus === 404,
    `status ${outsiderStatus}`);

  await fAlice.reload();
  await fAlice.waitForSelector('.blip .editor', { timeout: 15000 });
  await fAlice.waitForTimeout(800);
  check('attachments survive a reload, because they are part of the document',
    (await fAlice.locator('.embed-image img').count()) === 1 &&
      (await fAlice.locator('.embed-file').count()) === 3);
  check('and the inbox names the file rather than showing a blank box',
    !(await fAlice.textContent('.inbox-snippet')).includes('￼'));

  // --- surviving an outage ----------------------------------------------

  section('Surviving an outage');
  const frank = await signUp('frank');
  await createWave(frank, 'Resilience');
  await frank.click('.editor');
  await frank.keyboard.type('before the outage');
  await frank.waitForTimeout(700);

  stopServer();
  await sleep(1800);
  check('the connection is reported as lost',
    (await frank.getAttribute('#status', 'class')).includes('offline'));

  // Keep typing into a dead connection.
  await frank.click('.editor');
  await frank.keyboard.press('End');
  await frank.keyboard.type(' typed while the server was down');
  await frank.waitForTimeout(500);
  check('the editor keeps accepting input',
    (await frank.locator('.editor').first().textContent()).includes('server was down'));

  startServer();
  await sleep(8000);
  check('it reconnects on its own',
    (await frank.getAttribute('#status', 'class')).includes('online'),
    await frank.getAttribute('#status', 'class'));

  // The real question: did the offline typing reach storage?
  const verify = await signIn('frank');
  await verify.waitForSelector('.inbox-row', { timeout: 15000 });
  await verify.click('.inbox-row');
  await verify.waitForSelector('.blip .editor', { timeout: 15000 });
  await verify.waitForTimeout(1200);
  check('edits made during the outage are flushed and persisted',
    (await verify.locator('.editor').first().textContent()).trim() ===
      'before the outage typed while the server was down',
    await verify.locator('.editor').first().textContent());
} finally {
  await browser.close();
  stopServer();
  rmSync(dataDir, { recursive: true, force: true });
}

console.log(
  `\n${checks - failures}/${checks} checks passed` +
    (failures ? ` — ${failures} FAILED` : ''),
);
process.exit(failures === 0 ? 0 : 1);
