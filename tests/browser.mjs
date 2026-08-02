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

  // --- comments ---------------------------------------------------------

  section('Comments');
  const cAlice = await signUp('note_alice', { width: 1400, height: 900 });
  const cBob = await signUp('note_bob', { width: 1400, height: 900 });
  await createWave(cAlice, 'Release notes');
  await cAlice.click('.blip .editor');
  await cAlice.keyboard.type('We ship on Friday and tell nobody.');
  await cAlice.waitForTimeout(500);
  await addParticipant(cAlice, 'note_bob');
  await setMode(cAlice, 'notepad');
  await cBob.waitForSelector('.inbox-row', { timeout: 15000 });
  await cBob.click('.inbox-row:has-text("Release notes")');
  await cBob.waitForSelector('.blip .editor', { timeout: 15000 });
  await cBob.waitForTimeout(600);

  check('a notepad offers the comment control', await cAlice.isVisible('.tool-comment'));

  /// Select `word` inside the page editor, the way a person would.
  ///
  /// Deliberately able to span text nodes: once a phrase is commented it sits in
  /// its own element, so any selection *containing* a comment crosses a
  /// boundary — and that is exactly the case worth testing.
  const selectWord = async (page, word) =>
    page.evaluate((needle) => {
      const root = document.querySelector('.blip .editor');
      const at = root.textContent.indexOf(needle);
      if (at < 0) return false;
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      let seen = 0;
      let start = null;
      let end = null;
      while (walker.nextNode() && !(start && end)) {
        const node = walker.currentNode;
        const len = node.data.length;
        if (!start && seen + len >= at) start = { node, offset: at - seen };
        if (!end && seen + len >= at + needle.length) {
          end = { node, offset: at + needle.length - seen };
        }
        seen += len;
      }
      if (!start || !end) return false;
      const range = document.createRange();
      range.setStart(start.node, start.offset);
      range.setEnd(end.node, end.offset);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      root.dispatchEvent(new Event('mouseup', { bubbles: true }));
      return true;
    }, word);

  check('the phrase can be selected', await selectWord(cAlice, 'Friday'));
  await cAlice.click('.tool-comment');
  await cAlice.waitForSelector('.comment-card', { timeout: 10000 });
  check('commenting marks the text', (await cAlice.locator('.commented').count()) === 1);
  check('the marked text is the phrase that was selected',
    (await cAlice.textContent('.commented')) === 'Friday');
  check('and a card opens beside it', await cAlice.isVisible('.comment-card.active'));

  await cAlice.keyboard.type('Friday is too soon.');
  await cAlice.waitForTimeout(700);
  check('the remark is typed into the card',
    (await cAlice.textContent('.comment-card')).includes('Friday is too soon.'));

  await cBob.waitForSelector('.comment-card', { timeout: 15000 });
  await cBob.waitForTimeout(400);
  check('the comment reaches the other participant live',
    (await cBob.textContent('.comment-card')).includes('Friday is too soon.'));
  check('and so does the highlight', (await cBob.locator('.commented').count()) === 1);

  // The point of anchoring to the document rather than to an offset: Bob types
  // in front of the commented phrase and the highlight goes with the words.
  const cardTopBefore = await cAlice.evaluate(
    () => document.querySelector('.comment-card').getBoundingClientRect().top,
  );
  await cBob.click('.blip .editor');
  await cBob.keyboard.press('Home');
  await cBob.keyboard.type('Reminder to everyone:\n\n');
  await cBob.waitForTimeout(1200);
  check('the highlight follows its words when text is inserted above',
    (await cAlice.textContent('.commented')) === 'Friday');
  check('and the card moves down with them',
    (await cAlice.evaluate(
      () => document.querySelector('.comment-card').getBoundingClientRect().top,
    )) > cardTopBefore);

  // Typing at the end of a commented word must not drag the new letters into
  // somebody else's comment.
  await cAlice.click('.commented');
  await cAlice.waitForTimeout(300);
  await cAlice.keyboard.press('End');
  check('a comment does not spread into text typed after it',
    (await cAlice.textContent('.commented')) === 'Friday');

  // But typing *inside* the phrase must stay inside it. Leaving the anchor off
  // punches a hole through the middle of the highlight and splits one anchor
  // into two runs, which puts the card beside half the phrase it is about.
  //
  // Checked on Bob's screen, not Alice's. The typing fast path deliberately
  // leaves the browser's own DOM edit in place without re-rendering, so Alice's
  // markup would show one unbroken span even when the model underneath had been
  // split in two. Bob's copy is built from the operation, so it is the model.
  await selectWord(cAlice, 'day');
  await cAlice.keyboard.press('ArrowLeft');
  await cAlice.keyboard.type('nal-');
  await cBob.waitForTimeout(1400);
  check('an edit inside a commented phrase stays inside the comment',
    (await cBob.textContent('.commented')) === 'Frinal-day',
    await cBob.textContent('.commented'));
  check('and the phrase stays one unbroken highlight',
    (await cBob.locator('.commented').count()) === 1);
  for (let i = 0; i < 4; i += 1) await cAlice.keyboard.press('Backspace');
  await cBob.waitForTimeout(1400);
  check('undoing it leaves the anchor as it was',
    (await cBob.textContent('.commented')) === 'Friday');

  // A wider selection that swallows an existing anchor must be refused, not
  // silently overwrite it. attributesAt() reports nothing for a range that is
  // partly commented, so the obvious guard lets this straight through and
  // detaches the older thread while its words are still on the page.
  check('a range containing a comment can be selected',
    await selectWord(cAlice, 'on Friday and'));
  await cAlice.click('.tool-comment');
  await cAlice.waitForTimeout(900);
  check('commenting over an existing comment is refused',
    (await cAlice.locator('.comment-card').count()) === 1);
  check('and the original anchor survives',
    (await cAlice.textContent('.commented')) === 'Friday');

  await cBob.click('.comment-card');
  await cBob.waitForTimeout(300);
  await cBob.click('.comment-compose-input');
  await cBob.keyboard.type('Agreed, move it.');
  await cBob.keyboard.press('Enter');
  await cBob.waitForTimeout(1000);
  check('a reply joins the thread',
    (await cAlice.locator('.comment-remark').count()) === 2);
  check('the margin holds one card, not one per remark',
    (await cAlice.locator('.comment-card').count()) === 1);

  await cAlice.click('.comment-card .btn.link:has-text("Resolve")');
  await cAlice.waitForTimeout(900);
  check('resolving takes the card out of the margin',
    (await cAlice.locator('.comment-card').count()) === 0);
  check('and reaches the other participant',
    (await cBob.locator('.comment-card').count()) === 0);
  check('the highlight goes quiet but the text stays',
    (await cAlice.textContent('.blip .editor')).includes('Friday'));
  check('a resolved thread can be found again',
    await cAlice.isVisible('.comment-toggle'));

  await cAlice.click('.comment-toggle');
  await cAlice.waitForTimeout(400);
  check('showing resolved threads brings the whole thread back',
    (await cAlice.locator('.comment-remark').count()) === 2);
  await cAlice.click('.comment-card .btn.link:has-text("Reopen")');
  await cAlice.waitForTimeout(900);
  check('reopening restores it', (await cAlice.locator('.comment-card:not(.resolved)').count()) === 1);

  // Deleting the words leaves the discussion of why they were wrong.
  await cAlice.evaluate(() => {
    const root = document.querySelector('.blip .editor');
    const mark = root.querySelector('.commented');
    const range = document.createRange();
    range.selectNodeContents(mark);
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  });
  await cAlice.keyboard.press('Backspace');
  await cAlice.waitForTimeout(1000);
  check('deleting the commented words removes the highlight',
    (await cAlice.locator('.commented').count()) === 0);
  check('but keeps the thread, marked as detached',
    (await cAlice.locator('.comment-card.detached').count()) === 1);
  check('and says so', (await cAlice.textContent('.comment-detached')).length > 0);

  // A mode that takes no new comments still has to show, and settle, the ones
  // already there — mode changes are reversible and destroy nothing.
  await setMode(cAlice, 'document');
  await cAlice.waitForTimeout(700);
  check('leaving notepad keeps the threads on show',
    (await cAlice.locator('.comment-card').count()) === 1);
  check('but offers no way to start another', !(await cAlice.isVisible('.tool-comment')));
  check('a thread can still be settled', await cAlice.isVisible('.btn.link:has-text("Resolve")'));

  await setMode(cAlice, 'frozen');
  await cAlice.waitForTimeout(700);
  check('a frozen wave settles nothing', !(await cAlice.isVisible('.btn.link:has-text("Resolve")')));

  await setMode(cAlice, 'notepad');
  await cAlice.waitForTimeout(700);
  const survived = await cAlice.textContent('.comment-rail');
  check('a mode round trip loses no remark',
    survived.includes('Friday is too soon.') && survived.includes('Agreed, move it.'),
    survived.slice(0, 100));

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
  const drawn = await fAlice.evaluate(async () => {
    const img = document.querySelector('.embed-image img');
    if (!img) return false;
    if (!img.complete) await new Promise((r) => img.addEventListener('load', r, { once: true }));
    return img.naturalWidth > 0;
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

  // The browser will move an embed element itself — a drag inside the message
  // does it — and the text-diffing path would then read the moved embed back
  // as a literal U+FFFC and send an op replacing the attachment with it. The
  // picture would become an empty box for everyone, permanently. Simulated by
  // doing to the DOM exactly what such a drag does.
  const beforeSabotage = await fAlice.textContent('.blip .editor');
  await fAlice.evaluate(() => {
    const editor = document.querySelector('.blip .editor');
    editor.appendChild(document.createTextNode('￼'));
    editor.dispatchEvent(new InputEvent('input', { bubbles: true }));
  });
  await fAlice.waitForTimeout(700);
  check('an embed the browser moved behind our back is put back, not sent as text',
    (await fAlice.locator('.embed-image img').count()) === 1 &&
      (await fAlice.textContent('.blip .editor')) === beforeSabotage);
  await fBob.waitForTimeout(700);
  check('and nothing reached the other side',
    (await fBob.locator('.embed-image img').count()) === 1);

  // Copying a spreadsheet range or part of a page puts a bitmap on the
  // clipboard *beside* the text. Taking the file there silently discards what
  // was actually copied.
  await fAlice.click('.editor');
  await fAlice.keyboard.press('End');
  await fAlice.evaluate(() => {
    const data = new DataTransfer();
    data.setData('text/plain', ' pasted text');
    data.items.add(new File([new Uint8Array([1, 2, 3])], 'clip.png', { type: 'image/png' }));
    document.querySelector('.blip .editor')
      .dispatchEvent(new ClipboardEvent('paste', { bubbles: true, clipboardData: data }));
  });
  await fAlice.waitForTimeout(900);
  check('a paste carrying both text and a bitmap keeps the text',
    (await fAlice.textContent('.blip .editor')).includes('pasted text') &&
      (await fAlice.locator('.embed-image img').count()) === 1);

  await fAlice.reload();
  await fAlice.waitForSelector('.blip .editor', { timeout: 15000 });
  await fAlice.waitForTimeout(800);
  check('attachments survive a reload, because they are part of the document',
    (await fAlice.locator('.embed-image img').count()) === 1 &&
      (await fAlice.locator('.embed-file').count()) === 3);
  check('and the inbox names the file rather than showing a blank box',
    !(await fAlice.textContent('.inbox-snippet')).includes('￼'));

  // The toolbar is a single element that lives inside whichever message holds
  // the caret. A rebuild of the thread with focus elsewhere used to leave it
  // in the discarded subtree, taking every formatting control off the page.
  // Near the top-left, on the text: the middle of this message is the picture,
  // and clicking that follows the link rather than placing a caret.
  await fAlice.click('.blip .editor', { position: { x: 6, y: 8 } });
  await fAlice.waitForTimeout(300);
  check('the toolbar follows the caret into the message',
    (await fAlice.locator('.blip-tools .tool-bold').count()) === 1);
  await fAlice.click('.search-input');
  // A new *message* rebuilds the whole thread; an edit does not.
  await fBob.click('.thread-foot .btn.primary');
  await fAlice.waitForTimeout(1800);
  check('and survives a thread rebuild with the caret elsewhere',
    (await fAlice.locator('.toolbar .tool-bold').count()) === 1);

  // --- muting, leaving, and finding a hit --------------------------------

  section('Muting, leaving, and jumping to a search hit');

  const gwen = await signUp('gwen');
  const hana = await signUp('hana');
  await createWave(gwen, 'Roadmap');
  await addParticipant(gwen, 'hana');
  await gwen.click('.blip .editor');
  await gwen.keyboard.type('the aardvark milestone slips to March');
  await gwen.waitForTimeout(900);

  // Hana has an unread wave. Muting it keeps it and drops the count.
  await hana.waitForTimeout(900);
  await hana.click('.inbox-row');
  await hana.waitForSelector('.wave-actions');
  await hana.click('.inbox-back, .sidebar-toggle').catch(() => {});
  await hana.waitForTimeout(400);

  await gwen.click('.blip .editor');
  await gwen.keyboard.type(' — and again');
  await gwen.waitForTimeout(1200);

  const badgeBeforeMute = await hana.locator('.inbox-row .badge').count();
  await hana.click('.wave-actions .btn.ghost:has-text("Mute")');
  await hana.waitForTimeout(900);
  check('muting a wave clears its unread badge',
    badgeBeforeMute >= 0 && (await hana.locator('.inbox-row .badge').count()) === 0);
  check('and says the wave is muted rather than just going quiet',
    (await hana.locator('.muted-mark').count()) === 1);
  check('the button offers to undo it',
    (await hana.locator('.wave-actions .btn.ghost:has-text("Unmute")').count()) === 1);

  // Leaving is a thing the server always allowed and the client never offered.
  await hana.click('.wave-actions .btn.ghost:has-text("Leave")');
  await hana.waitForSelector('.dialog');
  // A destructive confirmation is `.btn.danger`, not `.btn.primary`.
  await hana.click('.dialog .btn.danger');
  await hana.waitForTimeout(1200);
  check('leaving a wave takes it out of the inbox',
    (await hana.locator('.inbox-row').count()) === 0);
  check('and the person still in it sees them go',
    (await gwen.locator('.participants .avatar').count()) === 1);

  // A search hit knows which message matched; it used to open the wave at the
  // top and leave you to find the line by eye.
  await gwen.click('.blip .editor');
  await gwen.keyboard.press('End');
  for (let i = 0; i < 12; i += 1) {
    await gwen.click('.thread-foot .btn.primary');
    await gwen.waitForTimeout(120);
  }
  await gwen.waitForTimeout(900);
  const lastBlip = await gwen.locator('.blip').last();
  await lastBlip.locator('.editor').click();
  await gwen.keyboard.type('pangolin');
  await gwen.waitForTimeout(1200);

  await gwen.fill('.search-input', 'pangolin');
  await gwen.waitForTimeout(1200);
  check('search finds the message',
    (await gwen.locator('.inbox-row .inbox-snippet mark').count()) >= 1);
  await gwen.click('.inbox-row');
  await gwen.waitForTimeout(1400);
  check('clicking the hit marks the message it matched',
    (await gwen.locator('.blip.revealed').count()) === 1);
  check('and it is the message that actually contains the word',
    (await gwen.textContent('.blip.revealed')).includes('pangolin'));

  // --- undo ---------------------------------------------------------------

  section('Undo');

  const uma = await signUp('uma');
  const uri = await signUp('uri');
  await createWave(uma, 'Undoable');
  await addParticipant(uma, 'uri');
  await uma.click('.blip .editor');
  await uma.keyboard.type('first sentence. ');
  await uma.waitForTimeout(1100); // past the coalescing window
  await uma.keyboard.type('second sentence.');
  await uma.waitForTimeout(700);

  const accel = process.platform === 'darwin' ? 'Meta' : 'Control';
  await uma.keyboard.press(`${accel}+z`);
  await uma.waitForTimeout(600);
  check('undo takes back the last run of typing',
    (await uma.textContent('.blip .editor')).trim() === 'first sentence.');
  check('and not the one before it',
    (await uma.textContent('.blip .editor')).includes('first'));

  await uma.keyboard.press(`${accel}+Shift+z`);
  await uma.waitForTimeout(600);
  check('redo puts it back',
    (await uma.textContent('.blip .editor')).includes('second sentence.'));

  // The undo reached the server as an ordinary op, so the other participant
  // sees it. A local-only undo would leave the two copies disagreeing.
  await uri.waitForSelector('.inbox-row', { timeout: 15000 });
  await uri.click('.inbox-row');
  await uri.waitForSelector('.blip .editor', { timeout: 15000 });
  await uri.waitForTimeout(900);
  check('the other participant sees the same text',
    (await uri.textContent('.blip .editor')).includes('second sentence.'));

  // The part that is easy to get wrong: somebody else edits, and the stored
  // undo now refers to offsets that have moved.
  await uri.click('.blip .editor');
  await uri.keyboard.press('Home');
  await uri.keyboard.type('URI WROTE THIS. ');
  await uri.waitForTimeout(1200);
  await uma.waitForTimeout(600);
  check('the remote insert arrives', (await uma.textContent('.blip .editor')).includes('URI WROTE'));

  await uma.click('.blip .editor');
  await uma.keyboard.press(`${accel}+z`);
  await uma.waitForTimeout(900);
  const afterUndo = (await uma.textContent('.blip .editor')).trim();
  check('undo after a remote edit still removes your own text',
    !afterUndo.includes('second sentence.'), afterUndo);
  check('and leaves the other person\'s text alone',
    afterUndo.includes('URI WROTE THIS.'), afterUndo);
  check('and the two clients still agree', await (async () => {
    await uri.waitForTimeout(900);
    return (await uri.textContent('.blip .editor')).trim() === afterUndo;
  })());

  // --- keyboard and focus ------------------------------------------------

  section('Reachable without a mouse');

  const kate = await signUp('kate');
  await createWave(kate, 'Keyboard');

  // Renaming was a click handler on an h1: not focusable, and no key opened it.
  const titleFocusable = await kate.evaluate(() => {
    const button = document.querySelector('.wave-title-button');
    if (!button) return false;
    button.focus();
    return document.activeElement === button;
  });
  check('the wave title can be focused', titleFocusable);
  await kate.keyboard.press('Enter');
  await kate.waitForSelector('.dialog', { timeout: 5000 });
  check('and Enter opens the rename dialog', await kate.isVisible('.dialog'));

  check('the dialog says it is one', (await kate.getAttribute('.dialog', 'role')) === 'dialog');
  check('and is labelled by its own heading', await kate.evaluate(() => {
    const dialog = document.querySelector('.dialog');
    const id = dialog.getAttribute('aria-labelledby');
    return Boolean(id && dialog.querySelector(`#${id}`));
  }));
  check('focus starts inside it', await kate.evaluate(() =>
    document.querySelector('.dialog').contains(document.activeElement)));

  // Tab must not walk out of a modal into the page behind it.
  for (let i = 0; i < 8; i += 1) await kate.keyboard.press('Tab');
  check('and Tab stays inside it', await kate.evaluate(() =>
    document.querySelector('.dialog').contains(document.activeElement)));

  await kate.keyboard.press('Escape');
  await kate.waitForTimeout(300);
  check('Escape closes it', !(await kate.isVisible('.dialog')));
  check('and focus goes back where it came from', await kate.evaluate(() =>
    document.activeElement === document.querySelector('.wave-title-button')));

  // confirmAction had no Escape handler at all and focused nothing. Someone
  // else has to be here first: leaving a wave you are alone in is refused with
  // an explanation rather than a dialog.
  await addParticipant(kate, 'alice');
  await kate.click('.wave-actions .btn.ghost:has-text("Leave")');
  await kate.waitForSelector('.dialog');
  check('a confirmation focuses its safe choice, not its destructive one',
    (await kate.evaluate(() => document.activeElement.textContent)) === 'Cancel');
  await kate.keyboard.press('Escape');
  await kate.waitForTimeout(300);
  check('and Escape dismisses it too', !(await kate.isVisible('.dialog')));

  check('a participant can be reached by keyboard', await kate.evaluate(() => {
    const button = document.querySelector('.participant-button');
    if (!button) return false;
    button.focus();
    return document.activeElement === button;
  }));

  // The editor sets outline:none, so the card has to show which one has the
  // caret. It used to shift the border by about two per cent of luminance.
  await kate.click('.blip .editor');
  await kate.waitForTimeout(200);
  const ring = await kate.evaluate(() => {
    const blip = document.querySelector('.blip');
    const style = getComputedStyle(blip);
    return { shadow: style.boxShadow, border: style.borderColor };
  });
  check('the message holding the caret is visibly marked',
    ring.shadow !== 'none' && ring.shadow !== '');

  // --- surviving an outage ----------------------------------------------

  section('Surviving an outage');
  const frank = await signUp('frank');
  await createWave(frank, 'Resilience');
  await frank.click('.editor');
  await frank.keyboard.type('before the outage');
  await frank.waitForTimeout(700);

  // A phone-sized second session. Below 860px the sidebar — and with it the
  // status dot that was the entire offline indication — is hidden while a wave
  // is open, so this is the viewport where being offline used to be invisible.
  const offlinePhone = await signUp('frankie', { width: 390, height: 780 });
  await createWave(offlinePhone, 'On a phone');
  await offlinePhone.waitForTimeout(500);

  stopServer();
  await sleep(1800);
  check('the connection is reported as lost',
    (await frank.getAttribute('#status', 'class')).includes('offline'));
  check('and said in words, not only as a coloured dot',
    await frank.isVisible('#offline-banner'));
  check('the banner says what it means for what you type',
    (await frank.textContent('#offline-banner')).includes('keep typing'));
  check('and it is a live region, so it is announced rather than just drawn',
    (await frank.getAttribute('#offline-banner', 'role')) === 'status');
  check('it is visible on a phone, where the sidebar is not',
    await offlinePhone.isVisible('#offline-banner') && !(await offlinePhone.isVisible('.sidebar')));
  // Being visible is not the same as being placed. The banner is fixed, so
  // without the layout giving up the space it is drawn straight over the
  // header — the wave's title and buttons, and the sidebar's.
  check('and it does not cover what it sits above',
    !(await frank.evaluate(() => {
      const banner = document.getElementById('offline-banner').getBoundingClientRect();
      return ['.sidebar-head', '.wave-head', '.wave-actions']
        .map((s) => document.querySelector(s))
        .filter(Boolean)
        .some((node) => node.getBoundingClientRect().top < banner.bottom);
    })));

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
  check('and the banner goes away again',
    !(await frank.isVisible('#offline-banner')));

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
