// Regenerate the screenshots in docs/screenshots, which the README relies on.
//
//   cargo build --release -p gal-server
//   npm install --prefix tests            # playwright-core and a browser
//   node tools/screenshots.mjs
//
// The images in the README are not drawings. Each one is a real client, driven
// through three browser sessions against a real server on a throwaway database,
// so a screenshot cannot claim a state the software cannot actually reach. That
// only holds if they can be regenerated when the UI changes, which is what this
// script is for.
//
// Two liberties are taken, both cosmetic and both noted where they happen: the
// per-message action buttons are revealed (they are hover-only, and a hover can
// only ever reach one message at a time), and the window is a fixed size so the
// images stay consistent between runs.

import { chromium } from 'playwright-core';
import { spawn, execSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const SHOTS = join(ROOT, 'docs/screenshots');
const PORT = Number(process.env.GAL_SHOT_PORT || 8119);
const BASE = `http://127.0.0.1:${PORT}`;
const PASSWORD = 'correct horse battery';

// Retina, and the same frame every time so the images stay comparable.
const VIEWPORT = { width: 1440, height: 700 };
const SCALE = 2;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const dataDir = mkdtempSync(join(tmpdir(), 'gal-shots-'));
const binary = join(ROOT, 'target/release/gal-server');
try {
  execSync(`test -x ${binary}`);
} catch {
  console.error('Build the server first:\n  cargo build --release -p gal-server');
  process.exit(1);
}

const server = spawn(binary, [], {
  env: { ...process.env, GAL_PORT: String(PORT), GAL_DB: join(dataDir, 'shots.db') },
  stdio: 'ignore',
});
await sleep(1500);

/** Locate a cached Playwright browser; see tests/browser.mjs for the rationale. */
function browserPath() {
  if (process.env.GAL_CHROMIUM) return process.env.GAL_CHROMIUM;
  const home = process.env.HOME || process.env.USERPROFILE || '';
  const caches = [
    join(home, 'Library/Caches/ms-playwright'),
    join(home, '.cache/ms-playwright'),
    join(process.env.LOCALAPPDATA || '', 'ms-playwright'),
  ].filter((c) => c && existsSync(c));
  for (const cache of caches) {
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

const browser = await chromium.launch({ executablePath: browserPath() });

async function signUp(name, displayName) {
  const context = await browser.newContext({
    viewport: VIEWPORT,
    deviceScaleFactor: SCALE,
    colorScheme: 'light',
  });
  const page = await context.newPage();
  page.on('pageerror', (e) => console.log(`  [${name}] uncaught: ${e.message}`));
  await page.goto(BASE);
  await page.waitForSelector('.auth-form');
  await page.fill('input[placeholder="username"]', name);
  await page.fill('input[placeholder="display name (optional)"]', displayName);
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
  await page.waitForSelector('.thread', { timeout: 15000 });
  await page.waitForTimeout(600);
}

async function addParticipant(page, name) {
  await page.click('.add-participant');
  await page.waitForSelector('.dialog input');
  await page.fill('.dialog input', name);
  await page.click('.dialog .btn.primary');
  await page.waitForTimeout(700);
}

async function openWave(page, title) {
  await page.click(`.inbox-row:has-text("${title}")`);
  await page.waitForSelector('.thread', { timeout: 15000 });
  await page.waitForTimeout(700);
}

async function setMode(page, id) {
  await page.selectOption('.mode-select', id);
  await page.waitForSelector('.dialog .btn.primary', { timeout: 5000 });
  await page.click('.dialog .btn.primary');
  await page.waitForTimeout(900);
}

/**
 * Show every message's actions at once. They are hover-only, and a mouse can
 * only be over one message at a time, so a screenshot of the affordances the
 * README describes needs all of them up together.
 *
 * Through the CSSOM rather than an injected stylesheet: the app is served under
 * `style-src 'self'`, and an injected `<style>` is refused — as it should be.
 * The inline opacity lasts until the next render of the thread, so call this
 * immediately before the shot.
 */
async function revealActions(page) {
  await page.evaluate(() => {
    for (const actions of document.querySelectorAll('.blip-actions')) {
      actions.style.opacity = '1';
    }
  });
}

/** Select a run of text inside a message, so the toolbar has something to act on. */
async function selectWord(page, selector, word) {
  await page.click(selector);
  await page.evaluate(({ selector: sel, word: needle }) => {
    const root = document.querySelector(sel);
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const at = node.textContent.indexOf(needle);
      if (at === -1) continue;
      const range = document.createRange();
      range.setStart(node, at);
      range.setEnd(node, at + needle.length);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      return;
    }
  }, { selector, word });
  await page.waitForTimeout(300);
}

/**
 * Type at a human pace.
 *
 * Not cosmetic: the client sends one operation at a time and folds everything
 * typed while an acknowledgement is outstanding into the next one, so typing
 * instantly turns a sentence into three or four operations. The playback
 * screenshot is a claim about the op log's granularity, and it should be one
 * the log actually supports.
 */
const type = async (page, text) => {
  await page.keyboard.type(text, { delay: 28 });
  await page.waitForTimeout(700);
};

const shot = async (page, name) => {
  await page.screenshot({ path: join(SHOTS, name) });
  console.log(`  wrote ${name}`);
};

try {
  // The usernames decide the participant colours — the server derives a hue
  // from the name — so changing them changes every avatar in every image.
  const alice = await signUp('alice', 'Alice Nakamura');
  const bob = await signUp('bob', 'Bob Ferreira');
  const carol = await signUp('carol', 'Carol Osei');

  // --- a wave, co-edited, threaded, with a private reply ----------------

  console.log('Launch plan');
  await createWave(alice, 'Launch plan — Gal 1.0');
  await addParticipant(alice, 'bob');
  await addParticipant(alice, 'carol');

  await alice.click('.blip .editor');
  await type(
    alice,
    'We ship on Friday. Remaining blockers are the migration runner and the ' +
      'release notes. I have the migration runner; it lands tonight.',
  );

  await openWave(carol, 'Launch plan');
  await carol.hover('.blip[data-depth="0"]');
  await carol.click('.blip[data-depth="0"] > .blip-head .blip-action:has-text("Reply")');
  await carol.waitForTimeout(700);
  await carol.click('.blip[data-depth="1"] .editor');
  await type(carol, 'Release notes are drafted — I need a paragraph on wave modes.');

  await openWave(bob, 'Launch plan');
  await bob.hover('.blip[data-depth="1"]');
  await bob.click('.blip[data-depth="1"] > .blip-head .blip-action:has-text("Reply")');
  await bob.waitForTimeout(700);
  await bob.click('.blip[data-depth="2"] .editor');
  await type(bob, "I'll write it. Five modes, one table.");

  // Bob edits Alice's message: this is the co-editing the README describes, and
  // it is what puts a second contributor on it.
  await bob.click('.blip[data-depth="0"] .editor', { position: { x: 4, y: 8 } });
  await bob.keyboard.press('Home');
  await type(bob, 'Heads up: ');

  await selectWord(alice, '.blip[data-depth="0"] .editor', 'Friday');
  await alice.click('.tool-bold');
  await alice.waitForTimeout(700);

  // A private reply, which Carol is never sent.
  await alice.hover('.blip[data-depth="0"]');
  await alice.click('.blip[data-depth="0"] > .blip-head .blip-action:has-text("Privately")');
  await alice.waitForSelector('.dialog input');
  await alice.fill('.dialog input', 'bob');
  await alice.click('.dialog .btn.primary');
  await alice.waitForSelector('.private-thread', { timeout: 10000 });
  await alice.click('.private-thread .editor');
  await type(alice, 'Between us: if the runner slips we move to Monday.');

  // Reopen for the contributor list, which is part of the wave snapshot rather
  // than something the server pushes on every keystroke.
  await alice.reload();
  await alice.waitForSelector('.blip .editor', { timeout: 15000 });
  await alice.waitForTimeout(900);

  // Bob's caret, seen from Alice's page.
  await bob.click('.blip[data-depth="2"] .editor');
  await bob.keyboard.press('End');
  await alice.waitForTimeout(900);

  await revealActions(alice);
  await shot(alice, 'wave.png');

  // --- playback ---------------------------------------------------------

  console.log('Playback');
  await alice.click('.btn.ghost:has-text("Playback")');
  await alice.waitForSelector('.playback-slider', { timeout: 10000 });
  const label = await alice.evaluate(() => {
    const slider = document.querySelector('.playback-slider');
    // A third of the way in: the first message is still being typed and the
    // replies below it do not exist yet.
    slider.value = String(Math.round(Number(slider.max) * 0.35));
    slider.dispatchEvent(new Event('input', { bubbles: true }));
    return document.getElementById('playback-label').textContent;
  });
  await alice.waitForTimeout(500);
  await shot(alice, 'playback.png');
  console.log(`  playback frame: ${label}`);
  await alice.click('.btn.ghost:has-text("Exit playback")');
  await alice.waitForTimeout(700);

  // --- the same wave as a chat, and then frozen -------------------------

  console.log('Standup');
  await createWave(alice, 'Standup');
  await addParticipant(alice, 'bob');
  await addParticipant(alice, 'carol');
  await setMode(alice, 'chat');
  await alice.waitForSelector('.composer-input', { timeout: 10000 });

  await openWave(bob, 'Standup');
  await openWave(carol, 'Standup');
  await bob.waitForSelector('.composer-input', { timeout: 10000 });
  await carol.waitForSelector('.composer-input', { timeout: 10000 });

  const say = async (page, text) => {
    await page.click('.composer-input');
    await type(page, text);
    await page.keyboard.press('Enter');
    await page.waitForTimeout(700);
  };

  // The first message went into the empty blip a new wave starts with; the rest
  // are sent through the composer, the way a channel is actually used.
  await alice.click('.blip .editor');
  await type(alice, 'Standup thread for the week of the 1.0 push.');
  await say(bob, 'Migration runner is merged. Green on CI.');
  // Deliberately two from Bob in a row: this is what a channel collapses under
  // one header, and the screenshot should show it happening.
  await say(bob, 'Nothing left on it but the changelog entry.');
  await say(carol, 'Release notes ready for review — modes section is the last hole.');
  await say(bob, 'Writing it now. Table of five, plus a note that switching is reversible.');
  await say(alice, 'Then we are on for Friday. Freezing this thread after standup.');

  // Bob's view: a participant, so the mode is a badge rather than a picker.
  // Resting on the grouped message shows what replaces the header there — the
  // send time, and the actions for a message of your own.
  await bob.click('.composer-input');
  await type(bob, 'Draft: nothing is destroyed by a mode switch…');
  await bob.hover('.blip.chat.grouped');
  await bob.waitForTimeout(400);
  await shot(bob, 'mode-chat.png');

  // And the creator's view once it is frozen. Long enough for the toast to go:
  // it sits in the same place as the line explaining the mode, and covering
  // that line is the one thing this image is for.
  await setMode(alice, 'frozen');
  await alice.mouse.move(700, 400);
  await alice.waitForTimeout(4000);
  await shot(alice, 'mode-frozen.png');

  console.log('\nDone. Check docs/screenshots, and the README text that describes them.');
} finally {
  await browser.close();
  server.kill('SIGKILL');
  rmSync(dataDir, { recursive: true, force: true });
}
