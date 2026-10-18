// Browser regression tests for widget availability, run against the real server binary.
// Usage: cargo build && (cd tests/browser && npm ci && npx playwright install chromium && npm test)
import { spawn } from 'node:child_process';
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const BIN = new URL('../../target/debug/open-rust-chatbot', import.meta.url).pathname;
const KB = new URL('../../examples/knowledge-base.md', import.meta.url).pathname;
const PORT = 8931;
const BASE = `http://127.0.0.1:${PORT}`;
let server;

async function startServer(tokenDelayMs = 5) {
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
  await page.route('**/health', (route) => route.fulfill({ status: 503, body: '{"ok":false}' }));
  await page.goto(BASE + '/');
  await page.waitForTimeout(1000);
  assert.equal(await page.isVisible('[data-testid=orc-bubble]'), false, 'bubble must be hidden when /health fails');
  await page.unroute('**/health');
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
