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

await run('AI disclosure is always shown in the chat panel', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  const disclosure = await page.textContent('.orc-disclosure');
  assert.match(disclosure, /AI assistant/);
});

await run('chat continues on the next page: transcript, open panel and server context restored', async (page) => {
  await startServer();
  const sessions = [];
  page.on('websocket', (ws) => ws.on('framereceived', (f) => {
    try {
      const v = JSON.parse(f.payload);
      if (v.type === 'session') sessions.push(v);
    } catch {}
  }));
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', 'What are your opening hours?');
  await page.press('.orc-input', 'Enter');
  await page.waitForFunction(() => document.body.textContent.includes('8am'), null, { timeout: 5000 });
  await page.reload(); // same tab, next page
  await page.waitForSelector('.orc-panel[data-open="true"]', { timeout: 10000 });
  const text = await page.textContent('.orc-messages');
  assert.match(text, /What are your opening hours\?/, 'visitor message restored');
  assert.match(text, /8am/, 'assistant reply restored');
  await new Promise((r) => setTimeout(r, 500));
  assert.equal(sessions.length >= 2, true, 'a session frame on each page');
  assert.equal(sessions[sessions.length - 1].resumed, true, 'server restored the conversation');
  assert.equal(sessions[sessions.length - 1].turns, 1);
  assert.equal(await page.locator('.orc-msg-assistant', { hasText: 'Hi! How can I help' }).count(), 1, 'greeting restored once, not repeated');
});

async function chatOnce(page, question = 'What are your opening hours?') {
  await page.click('[data-testid=orc-bubble]');
  await page.fill('.orc-input', question);
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
}

await run('after a server restart the old transcript is replaced, not silently forgotten', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await chatOnce(page);
  stopServer();
  await startServer(); // parked chats live in memory, so a restart loses them
  await page.reload();
  await page.waitForFunction(() => document.body.textContent.includes('Your earlier chat has ended'), null, { timeout: 10000 });
  const text = await page.textContent('.orc-messages');
  assert.doesNotMatch(text, /opening hours/, 'old messages cleared');
});

await run('a closed panel stays closed on the next page, and phones get the button not the panel', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await chatOnce(page);
  await page.keyboard.press('Escape');
  await page.reload();
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible' });
  await page.waitForTimeout(800);
  assert.equal(await page.getAttribute('.orc-panel', 'data-open'), 'false', 'closed stays closed');

  await page.click('[data-testid=orc-bubble]'); // open, then move page on a phone
  await page.setViewportSize({ width: 390, height: 800 });
  await page.reload();
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible' });
  await page.waitForTimeout(800);
  assert.equal(await page.getAttribute('.orc-panel', 'data-open'), 'false', 'no auto-reopen on phones');
});

await run('a tampered saved transcript renders as text, never as HTML', async (page) => {
  await startServer();
  // Planted before the page's own scripts run, as an attacker with storage access could.
  await page.addInitScript(() => {
    sessionStorage.setItem('orc-transcript', JSON.stringify([
      { r: 'assistant', t: '<img src=x onerror="window.__pwned=1">' },
      { r: 'user', t: '<script>window.__pwned=2</script>' },
      { r: 'evil', t: 'ignored role' },
    ]));
  });
  await page.goto(BASE + '/');
  // Restored at page load, before any connection (which would then replace it).
  await page.waitForSelector('.orc-root', { state: 'attached' });
  await page.waitForTimeout(300);
  assert.equal(await page.evaluate(() => window.__pwned), undefined);
  assert.equal(await page.locator('.orc-messages img').count(), 0);
  assert.match(await page.textContent('.orc-messages'), /<img src=x/);
  assert.doesNotMatch(await page.textContent('.orc-messages'), /ignored role/);
});

await run('a reconnect on the same page keeps the server-side conversation', async (page) => {
  await startServer(5, { IDLE_TIMEOUT_SECONDS: '1' });
  const sessions = [];
  page.on('websocket', (ws) => ws.on('framereceived', (f) => {
    try {
      const v = JSON.parse(f.payload);
      if (v.type === 'session') sessions.push(v);
    } catch {}
  }));
  await page.goto(BASE + '/');
  await chatOnce(page);
  await page.waitForTimeout(2500); // server closes the idle socket
  await page.fill('.orc-input', 'And on Saturday?');
  await page.press('.orc-input', 'Enter');
  await page.waitForSelector('.orc-send:not([disabled])', { timeout: 5000 });
  const last = sessions[sessions.length - 1];
  assert.equal(last.resumed, true, 'the new socket resumed the conversation');
  assert.equal(last.turns, 1);
});

// Multi-site mode: a fake sites API also serves the host page, so the page's origin is the
// bot's allowed origin.
const { createServer } = await import('node:http');
const SITES_TOKEN = 'browser-test-token-0123456789abcdef0123'; // gitleaks:allow (test fixture)
const SITES_PORT = PORT + 1;
const HOST = `http://127.0.0.1:${SITES_PORT}`;
const demoBot = {
  id: 'demo-bot',
  status: 'active',
  name: 'Demo Co',
  botName: 'Demo Bot',
  title: 'Demo Co Chat',
  subtitle: 'Demo subtitle',
  greeting: 'Hello from the demo bot!',
  placeholder: 'Ask Demo Co...',
  privacyUrl: 'https://demo.example/privacy',
  allowedOrigins: [HOST],
  instructions: 'You are {bot_name}. {lead_instructions}',
  knowledgeBase: '# Opening hours\n\nDemo Co opens at 7am every day.',
  leadCapture: true,
  updatedAt: '2026-10-19T17:00:00.000Z',
};

function startSites() {
  const sites = createServer((req, res) => {
    if (req.url.startsWith('/host')) {
      const opts = req.url.includes('withTitle') ? "<script>window.openRustChatbot = { title: 'Host page title' };</script>" : '';
      res.writeHead(200, { 'content-type': 'text/html' });
      return res.end(`<!doctype html><title>host</title>${opts}<script src="${BASE}/b/demo-bot/widget.js" defer></script>`);
    }
    if (req.headers.authorization !== `Bearer ${SITES_TOKEN}`) {
      res.writeHead(401);
      return res.end();
    }
    if (req.url === '/internal/bots/demo-bot') {
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify(demoBot));
    }
    res.writeHead(404, { 'content-type': 'application/json' });
    res.end('{"error":"not_found"}');
  });
  return new Promise((resolve) => sites.listen(SITES_PORT, '127.0.0.1', () => resolve(sites)));
}

const multiSiteEnv = { SITES_API_URL: `${HOST}/internal/bots`, INTERNAL_API_TOKEN: SITES_TOKEN };

await run('multi-site: widget from /b/<id>/widget.js applies the bot settings and chats with that bot', async (page) => {
  const sites = await startSites();
  try {
    await startServer(5, multiSiteEnv);
    await page.goto(HOST + '/host');
    await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
    await page.click('[data-testid=orc-bubble]');
    assert.equal(await page.textContent('.orc-header-title'), 'Demo Co Chat');
    assert.equal(await page.textContent('.orc-header-subtitle'), 'Demo subtitle');
    assert.equal(await page.getAttribute('.orc-input', 'placeholder'), 'Ask Demo Co...');
    assert.equal(await page.getAttribute('.orc-disclosure-link', 'href'), 'https://demo.example/privacy');
    await page.waitForFunction(() => document.body.textContent.includes('Hello from the demo bot!'), null, { timeout: 5000 });
    await page.fill('.orc-input', 'What are your opening hours?');
    await page.press('.orc-input', 'Enter');
    await page.waitForFunction(() => document.body.textContent.includes('Demo Co opens at 7am'), null, { timeout: 5000 });
  } finally {
    sites.close();
  }
});

await run('multi-site: options set by the host page win over the bot settings', async (page) => {
  const sites = await startSites();
  try {
    await startServer(5, multiSiteEnv);
    await page.goto(HOST + '/host?withTitle');
    await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
    await page.click('[data-testid=orc-bubble]');
    assert.equal(await page.textContent('.orc-header-title'), 'Host page title');
    await page.waitForFunction(() => document.body.textContent.includes('Hello from the demo bot!'), null, { timeout: 5000 });
  } finally {
    sites.close();
  }
});

await run('single-site: /config has no texts, so the defaults stay', async (page) => {
  await startServer();
  await page.goto(BASE + '/');
  await page.click('[data-testid=orc-bubble]');
  assert.equal(await page.textContent('.orc-header-title'), 'Chat with us');
  await page.waitForFunction(() => document.body.textContent.includes('Hi! How can I help you today?'), null, { timeout: 5000 });
});

await run('multi-site: a slow /config never holds the widget back; texts update when it arrives', async (page) => {
  const sites = await startSites();
  try {
    await startServer(5, multiSiteEnv);
    await page.route('**/b/demo-bot/config', async (route) => {
      await new Promise((r) => setTimeout(r, 2500));
      await route.continue();
    });
    await page.goto(HOST + '/host');
    await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 1500 });
    await page.click('[data-testid=orc-bubble]');
    assert.equal(await page.textContent('.orc-header-title'), 'Chat with us', 'defaults first');
    await page.waitForFunction(() => document.querySelector('.orc-header-title').textContent === 'Demo Co Chat', null, { timeout: 5000 });
    await page.waitForFunction(() => document.body.textContent.includes('Hello from the demo bot!'), null, { timeout: 5000 });
    assert.equal(await page.locator('.orc-msg-assistant').count(), 1, 'greeting replaced, not duplicated');
  } finally {
    sites.close();
  }
});
