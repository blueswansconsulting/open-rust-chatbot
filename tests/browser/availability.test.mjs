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
  avatar: 'directory',
  avatarMotion: 'wave',
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
    // The bot's mascot comes from the server root, not from /b/<id>/.
    await page.waitForSelector('[data-testid=orc-bubble] svg .orc-hand', { timeout: 5000 });
    assert.equal(await page.getAttribute('.orc-header-avatar img', 'src'), BASE + '/avatars/directory.svg');
    assert.equal(await page.locator('.orc-root.orc-wave-go').count(), 1);
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

await run('mascot avatar fills the launcher and follows the pointer', async (page) => {
  await startServer();
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: 'electrician', avatarMotion: 'follow' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble] svg .orc-head', { timeout: 5000 });
  assert.equal(await page.locator('[data-testid=orc-bubble] script').count(), 0);
  await page.mouse.move(5, 5);
  await page.waitForTimeout(400);
  const turned = await page.getAttribute('[data-testid=orc-bubble] .orc-head', 'transform');
  assert.match(turned || '', /rotate\(-/, 'head turns toward a pointer on the left');
  assert.equal(await page.getAttribute('.orc-header-avatar img', 'src'), BASE + '/avatars/electrician.svg');
  assert.equal(await page.getAttribute('[data-testid=orc-bubble]', 'aria-label'), 'Open chat');
});

await run('reduced motion keeps the mascot still', async (page) => {
  await startServer();
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: 'electrician', avatarMotion: 'follow' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble] svg .orc-head', { timeout: 5000 });
  await page.mouse.move(5, 5);
  await page.waitForTimeout(400);
  assert.equal(await page.getAttribute('[data-testid=orc-bubble] .orc-head', 'transform'), null);
});

await run('an avatar name that is not a bundled mascot keeps the chat icon', async (page) => {
  await startServer();
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: '../widget', avatarMotion: 'follow' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
  await page.waitForTimeout(500);
  assert.equal(await page.locator('[data-testid=orc-bubble].orc-bubble-avatar').count(), 0);
  assert.equal(await page.locator('.orc-header-avatar').count(), 0);
});

await run('an unknown bundled name falls back to the chat icon', async (page) => {
  await startServer();
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: 'nobody', avatarMotion: 'still' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
  await page.waitForFunction(() => !document.querySelector('.orc-bubble-avatar'), null, { timeout: 5000 });
  assert.equal(await page.locator('[data-testid=orc-bubble] svg path').count(), 1);
});

await run('follow mode stops touching the page once the head is at rest', async (page) => {
  await startServer();
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: 'electrician', avatarMotion: 'follow' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble] svg .orc-head', { timeout: 5000 });
  await page.mouse.move(5, 5);
  await page.waitForTimeout(300);
  assert.match((await page.getAttribute('[data-testid=orc-bubble] .orc-head', 'transform')) || '', /rotate/);
  // 2.5s idle, then it eases back to centre and stops.
  await page.waitForTimeout(4500);
  const changes = await page.evaluate(
    () =>
      new Promise((resolve) => {
        let n = 0;
        const obs = new MutationObserver((m) => (n += m.length));
        obs.observe(document.querySelector('[data-testid=orc-bubble]'), { attributes: true, subtree: true });
        setTimeout(() => (obs.disconnect(), resolve(n)), 1000);
      }),
  );
  assert.equal(changes, 0, 'no per-frame work while idle');
  assert.equal(await page.getAttribute('[data-testid=orc-bubble] .orc-head', 'transform'), null);
});

await run('the inline mascot keeps only plain shapes, whatever the SVG holds', async (page) => {
  await startServer();
  await page.route('**/avatars/electrician.svg', (route) =>
    route.fulfill({
      contentType: 'image/svg+xml',
      body:
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" onload="window.pwned=1">' +
        '<style>body{display:none}</style><script>window.pwned=2</script>' +
        '<g class="orc-head"><circle r="3" fill="url(https://evil.test/x)" style="x"/>' +
        '<g class="orc-face"/><g class="orc-hat"/></g>' +
        '<foreignObject><iframe xmlns="http://www.w3.org/1999/xhtml" srcdoc="&lt;script&gt;parent.pwned=3&lt;/script&gt;"/></foreignObject>' +
        '<a href="javascript:alert(1)"><path d="M0 0"/></a><set attributeName="href" to="javascript:1"/>' +
        '<iframe xmlns="http://www.w3.org/1999/xhtml" srcdoc="x"/></svg>',
    }),
  );
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: 'electrician', avatarMotion: 'follow' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('[data-testid=orc-bubble] svg .orc-head', { timeout: 5000 });
  await page.waitForTimeout(300);
  const html = await page.innerHTML('[data-testid=orc-bubble]');
  for (const bad of ['onload', 'style', 'script', 'iframe', 'foreignObject', 'href', '<set', 'url(', '<a']) {
    assert.ok(!html.includes(bad), `${bad} was stripped: ${html}`);
  }
  assert.equal(await page.evaluate(() => window.pwned), undefined);
  assert.equal(await page.isVisible('body'), true);
});

await run('the wave plays once per tab, not on the next page', async (page) => {
  await startServer();
  await page.addInitScript(() => {
    window.openRustChatbot = { avatar: 'solar', avatarMotion: 'wave' };
  });
  await page.goto(BASE + '/');
  await page.waitForSelector('.orc-root.orc-wave-go .orc-hand', { timeout: 5000 });
  // 2.5s delay + 1.8s wave, then the class goes so a reappearing button doesn't wave again.
  await page.waitForSelector('.orc-root:not(.orc-wave-go) [data-testid=orc-bubble] svg', { timeout: 8000 });
  await page.reload();
  await page.waitForSelector('[data-testid=orc-bubble] svg .orc-hand', { timeout: 5000 });
  await page.waitForTimeout(300);
  assert.equal(await page.locator('.orc-root.orc-wave-go').count(), 0);
});

await run('a host page can turn the bot avatar off', async (page) => {
  const sites = await startSites();
  try {
    await startServer(5, multiSiteEnv);
    await page.addInitScript(() => {
      window.openRustChatbot = { avatar: 'none' };
    });
    await page.goto(HOST + '/host');
    await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
    await page.click('[data-testid=orc-bubble]');
    await page.waitForFunction(() => document.body.textContent.includes('Hello from the demo bot!'), null, { timeout: 5000 });
    assert.equal(await page.locator('.orc-bubble-avatar, .orc-header-avatar').count(), 0);
  } finally {
    sites.close();
  }
});

// A tall host page with schema.org data, served from the chatbot's own origin.
async function hostPage(page, path, opts, { jsonLd = true } = {}) {
  const ld = jsonLd
    ? '<script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"Service","name":"Rewires","areaServed":{"@type":"Place","name":"Bolton"}}]}</script>'
    : '';
  await page.route(BASE + path, (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><title>host</title>${ld}<script>window.openRustChatbot=${JSON.stringify(opts)}</script>` +
        `<div style="height:4000px">content</div><script src="${BASE}/widget.js" defer></script>`,
    }),
  );
  await page.goto(BASE + path);
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
}

const teaserOpts = {
  attention: 'teaser',
  teaser: 'Need help with {service} in {town}?',
  teaserFallback: 'Need an electrician? Ask me.',
};

await run('the teaser names the page’s town and service, never takes focus, and opens the chat', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', teaserOpts);
  await page.focus('body');
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  assert.equal(await page.textContent('.orc-teaser-text'), 'Need help with Rewires in Bolton?');
  assert.equal(await page.evaluate(() => document.activeElement === document.body), true);
  await page.click('.orc-teaser-text');
  assert.equal(await page.getAttribute('.orc-panel', 'data-open'), 'true');
  await page.waitForSelector('[data-testid=orc-teaser]', { state: 'detached', timeout: 2000 });
});

await run('without town or service data the fallback is used', async (page) => {
  await startServer();
  await hostPage(page, '/about', teaserOpts, { jsonLd: false });
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  assert.equal(await page.textContent('.orc-teaser-text'), 'Need an electrician? Ask me.');
});

await run('a closed teaser stays closed on the next page, and shows once per tab', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  await page.click('.orc-teaser-close');
  await page.waitForSelector('[data-testid=orc-teaser]', { state: 'detached', timeout: 2000 });
  // Like a new tab (fresh sessionStorage): the close is still remembered.
  await page.evaluate(() => sessionStorage.clear());
  await hostPage(page, '/electricians/bury', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForTimeout(1200);
  assert.equal(await page.locator('[data-testid=orc-teaser]').count(), 0);
});

await run('the teaser shows once per tab, even without a close', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  await hostPage(page, '/electricians/bury', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForTimeout(1200);
  assert.equal(await page.locator('[data-testid=orc-teaser]').count(), 0);
});

await run('the teaser and button show above a site’s own cookie banner', async (page) => {
  await startServer();
  await page.route(BASE + '/with-bar', (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><title>host</title><script>window.openRustChatbot=${JSON.stringify(teaserOpts)}</script>` +
        '<div style="height:4000px">content</div>' +
        `<script src="${BASE}/widget.js" defer></script>` +
        '<div style="position:fixed;left:12px;right:12px;bottom:12px;height:218px;background:#eee;z-index:2147483647">Cookies</div>',
    }),
  );
  await page.goto(BASE + '/with-bar');
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  // The chat rose above the banner, and goes back down once it's answered.
  const bannerTop = await page.evaluate(() => document.querySelector('div[style*="position:fixed"]').getBoundingClientRect().top);
  assert.ok((await page.locator('[data-testid=orc-bubble]').boundingBox()).y + 64 <= bannerTop + 1, 'button is above the banner');
  // The topmost element at the button's and teaser's centres is ours, not the banner.
  for (const sel of ['[data-testid=orc-bubble]', '.orc-teaser-text']) {
    const box = await page.locator(sel).boundingBox();
    const ours = await page.evaluate(([x, y]) => !!document.elementFromPoint(x, y).closest('.orc-root'), [box.x + box.width / 2, box.y + box.height / 2]);
    assert.ok(ours, `${sel} is hidden behind the banner`);
  }
  await page.evaluate(() => document.querySelector('div[style*="position:fixed"]').remove());
  await page.waitForFunction(() => !document.querySelector('.orc-root').style.bottom, null, { timeout: 3000 });
});

await run('the close button and message are comfortable to tap', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  for (const sel of ['.orc-teaser-close', '.orc-teaser-text']) {
    const box = await page.locator(sel).boundingBox();
    assert.ok(box.height >= 44, `${sel} is ${box.height}px tall`);
  }
  // Laid out as a bubble beside the button, not squeezed into the button's 64px column.
  const bubbleBox = await page.locator('[data-testid=orc-teaser]').boundingBox();
  assert.ok(bubbleBox.width >= 200, `teaser is only ${bubbleBox.width}px wide`);
  assert.ok(bubbleBox.height <= 80, `teaser is ${bubbleBox.height}px tall`);
  assert.ok((await page.locator('.orc-teaser-close').boundingBox()).width >= 44);
});

await run('no prompts on privacy or contact pages', async (page) => {
  await startServer();
  await hostPage(page, '/privacy', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForTimeout(1200);
  assert.equal(await page.locator('[data-testid=orc-teaser]').count(), 0);
});

await run('the button shows its label for a few seconds, once, then shrinks back', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', { attention: 'label', label: 'Get a call back' });
  await page.mouse.wheel(0, 400);
  await page.waitForSelector('.orc-bubble-labelled .orc-bubble-label', { timeout: 5000 });
  assert.equal(await page.getAttribute('[data-testid=orc-bubble]', 'aria-label'), 'Get a call back, open chat');
  await page.waitForSelector('.orc-bubble-labelled', { state: 'detached', timeout: 8000 });
  assert.equal(await page.getAttribute('[data-testid=orc-bubble]', 'aria-label'), 'Open chat');
  await page.mouse.wheel(0, 400);
  await page.waitForTimeout(800);
  assert.equal(await page.locator('.orc-bubble-labelled').count(), 0);
});

await run('attention is off unless asked for', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', { teaser: 'Hi {town}' });
  await page.mouse.wheel(0, 2000);
  await page.waitForTimeout(1200);
  assert.equal(await page.locator('[data-testid=orc-teaser], .orc-bubble-labelled').count(), 0);
});

await run('screen readers hear the teaser: its live region is not inside the closed panel', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  await page.waitForFunction(() => document.querySelector('.orc-sr-only[aria-live]').textContent.includes('Bolton'), null, { timeout: 2000 });
  assert.equal(await page.evaluate(() => !document.querySelector('.orc-sr-only[aria-live]').closest('.orc-panel')), true);
});

await run('Escape dismisses the teaser for a week and focus moves to the chat button', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', teaserOpts);
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  await page.focus('.orc-teaser-close');
  await page.keyboard.press('Escape');
  await page.waitForSelector('[data-testid=orc-teaser]', { state: 'detached', timeout: 2000 });
  assert.equal(await page.evaluate(() => document.activeElement.dataset.testid), 'orc-bubble');
  assert.ok(await page.evaluate(() => Number(localStorage.getItem('orc-teaser-closed')) > Date.now()));
});

await run('a business-wide list of towns names none of them; the page’s own place wins', async (page) => {
  await startServer();
  let opts = teaserOpts;
  await page.route(BASE + '/bury', (route) =>
    route.fulfill({
      contentType: 'text/html',
      body:
        '<!doctype html><title>host</title><script type="application/ld+json">' +
        JSON.stringify({ '@context': 'https://schema.org', '@graph': [
          { '@type': 'Electrician', name: 'Sparks', areaServed: [{ '@type': 'City', name: 'Bolton' }, { '@type': 'City', name: 'Bury' }],
            hasOfferCatalog: { itemListElement: [{ '@type': 'Service', name: 'Catalogue item' }] } },
          { '@type': 'WebPage', about: { '@type': 'City', name: 'Bury' } },
        ] }) +
        `</script><script>window.openRustChatbot=${JSON.stringify(opts)}</script>` +
        `<div style="height:4000px">content</div><script src="${BASE}/widget.js" defer></script>`,
    }),
  );
  await page.goto(BASE + '/bury');
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  // No page-level Service, so the {service} text can't be filled: the fallback is used.
  assert.equal(await page.textContent('.orc-teaser-text'), 'Need an electrician? Ask me.');
  // With a town-only teaser, the page's own place is used, not the first of the business's towns.
  await page.evaluate(() => sessionStorage.clear());
  opts = { attention: 'teaser', teaser: 'Need an electrician in {town}?', teaserFallback: 'x' };
  await page.reload();
  await page.waitForSelector('[data-testid=orc-bubble]', { state: 'visible', timeout: 5000 });
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 5000 });
  assert.equal(await page.textContent('.orc-teaser-text'), 'Need an electrician in Bury?');
});

await run('with both, the teaser waits until well after the label', async (page) => {
  await startServer();
  await hostPage(page, '/electricians/bolton', { ...teaserOpts, attention: 'both', label: 'Get a call back' });
  await page.mouse.wheel(0, 2000);
  await page.waitForSelector('.orc-bubble-labelled', { timeout: 5000 });
  await page.waitForTimeout(1000);
  assert.equal(await page.locator('[data-testid=orc-teaser]').count(), 0, 'no teaser while the label shows');
  await page.waitForSelector('[data-testid=orc-teaser]', { timeout: 15000 });
  assert.equal(await page.locator('.orc-bubble-labelled').count(), 0);
});
