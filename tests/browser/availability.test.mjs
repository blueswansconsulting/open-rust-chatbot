// Browser regression tests for widget availability, run against the real server binary.
// Usage: cargo build && (cd tests/browser && npm ci && npx playwright install chromium && npm test)
import { spawn } from 'node:child_process';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const BIN = new URL('../../target/debug/open-rust-chatbot', import.meta.url).pathname;
const KB = new URL('../../examples/knowledge-base.md', import.meta.url).pathname;
const PORT = Number(process.env.BROWSER_TEST_PORT || 8931);
const BASE = `http://127.0.0.1:${PORT}`;
let server;

async function startServer(tokenDelayMs = 5, extraEnv = {}) {
  server = spawn(BIN, [], {
    env: {
      ...process.env,
      DEV_MODE: 'true',
      MOCK_LLM: 'true',
      MOCK_TOKEN_DELAY_MS: String(tokenDelayMs),
      HOST: '127.0.0.1',
      PORT: String(PORT),
      ALLOWED_ORIGINS: BASE,
      KNOWLEDGE_BASE_PATH: KB,
      ...extraEnv,
    },
    stdio: 'ignore',
  });
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${BASE}/health`)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error('server did not start');
}

function stopServer() {
  server?.kill('SIGKILL');
  server = null;
}

async function run(name, fn) {
  const browser = await chromium.launch();
  try {
    await fn(await browser.newPage());
    console.log(`ok - ${name}`);
  } catch (e) {
    console.error(`FAIL - ${name}\n${e.stack}`);
    process.exitCode = 1;
  } finally {
    await browser.close();
    stopServer();
  }
}

await run('button shows when the chat is healthy', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
});

await run('button stays hidden while the chat is down, and appears when it recovers', async (page) => {
  await startServer();
  await page.route('**/availability', (route) => route.fulfill({ status: 503, body: '{"ok":false}' }));
  await page.goto(BASE + '/');
  await page.waitForTimeout(1000);
  assert.equal(await page.isVisible('[data-testid=orc-bubble]'), false, 'bubble must be hidden when /availability fails');
  await page.unroute('**/availability');
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
});

await run('connection lost mid-reply: apology, paused input, automatic reconnect', async (page) => {
  await startServer(400); // slow replies so the server can die mid-answer
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', 'Hello there');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-typing', { timeout: 3000 });
  stopServer();
  await page.waitForFunction(() => document.body.textContent.includes('Sorry, we lost the connection'), null, { timeout: 5000 });
  assert.equal(await page.isDisabled('.orc-input'), true, 'input pauses during the outage');
  await startServer();
  await page.waitForFunction(() => document.body.textContent.includes("You're reconnected. Please send your message again."), null, { timeout: 15000 });
  assert.equal(await page.isDisabled('.orc-input'), false, 'input resumes after reconnecting');
  await page.fill('.orc-input', 'Are you back?');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 10000 });
  const last = await page.$$eval('.orc-msg', (els) => {
    const e = els[els.length - 1];
    return { role: e.getAttribute('data-role'), text: e.textContent };
  });
  assert.equal(last.role, 'assistant', 'a normal reply arrives after reconnecting');
  assert.doesNotMatch(last.text, /reconnect/i);
});

await run('connection lost while idle in an open chat: apology, then reconnected', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', 'Hi');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
  stopServer();
  await page.waitForFunction(() => document.body.textContent.includes('Sorry, the chat connection has dropped'), null, { timeout: 5000 });
  await startServer();
  await page.waitForFunction(() => document.body.textContent.includes("You're reconnected. Carry on"), null, { timeout: 15000 });
});

const text = (page) => page.evaluate(() => document.body.textContent);
const count = (s, needle) => s.split(needle).length - 1;

await run('server refuses the connection while up: one clear message, no false "reconnected"', async (page) => {
  await startServer(5, { ALLOWED_ORIGINS: 'http://not-this-site.example' });
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.waitForFunction(() => document.body.textContent.includes("can't connect right now"), null, { timeout: 5000 });
  for (const msg of ['first', 'second', 'third']) {
    await page.waitForSelector('.orc-input:not([disabled])', { timeout: 5000 });
    await page.fill('.orc-input', msg);
    await page.press('.orc-input', 'Enter');
    await page.waitForTimeout(800);
  }
  const t = await text(page);
  assert.equal(count(t, 'reconnected'), 0, 'never claims to be reconnected');
  assert.ok(count(t, "couldn't send your message") >= 1, 'the visitor is told the message was not sent');
  await page.keyboard.press('Escape');
  assert.equal(await page.isVisible('[data-testid=orc-bubble]'), false, 'button hides after repeated refusals');
});

await run('server closing an idle socket stays silent', async (page) => {
  await startServer(5, { IDLE_TIMEOUT_SECONDS: '1' });
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', 'Hi');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
  await page.waitForTimeout(3000); // the server closes the idle socket after 1s
  const t = await text(page);
  assert.equal(count(t, 'Sorry'), 0, 'no apology for a harmless idle close');
  await page.fill('.orc-input', 'Still there?');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
  assert.equal(count(await text(page), 'Sorry'), 0);
});

await run('closing the panel during an outage hides the button until the chat is back', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', 'Hi');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
  stopServer();
  await page.waitForFunction(() => document.body.textContent.includes('Sorry, the chat connection has dropped'), null, { timeout: 5000 });
  await page.keyboard.press('Escape');
  assert.equal(await page.isVisible('[data-testid=orc-bubble]'), false, 'no button while the chat is down');
  assert.equal(await page.getAttribute('[data-testid=orc-bubble]', 'aria-expanded'), 'false');
  await startServer();
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 20000 });
});

await run('one failed health check while connected shows nothing', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', 'Hi');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
  let blips = 1;
  await page.route('**/availability', (route) => (blips-- > 0 ? route.abort() : route.continue()));
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await page.waitForTimeout(1500);
  assert.equal(count(await text(page), 'Sorry'), 0, 'a single blip must not interrupt the chat');
});
