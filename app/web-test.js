// Browser tests for the web host (run from app/ after `nsh package --target web -o dist`).
//   (a) demo mode: a transcript row and the status line show as DOM text.
//   (b) live mode: a scripted WebSocket server (below, RFC 6455 by hand) sends
//       welcome, a snapshot and a streamed delta; typing + Enter sends a submit.
//   (p1..p5) owner portal: sign-in, the session list, open and offline rows,
//       sign-out, and an invite created, shown and revoked from Settings.
// The page is served by a Node site (startSite) on one origin: the built
// bundle under / and /app/, and the portal's routes (/api/*, /auth) answered
// from a scripted state. Uses only Node built-ins and the preinstalled
// Playwright (headless Chromium).
const { chromium } = require('playwright');
const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const net = require('net');
const path = require('path');

const DIST = path.join(__dirname, 'dist', 'Modron');
const SESSION = '5b1e8f4a-2c3d-4e5f-8a9b-0c1d2e3f4a5b';
const COOKIE = 'owner-cookie-for-tests';
const TOKEN = 'correct-horse-token';
const MIME = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css',
  '.wasm': 'application/wasm', '.json': 'application/json', '.webmanifest': 'application/manifest+json',
  '.bin': 'application/octet-stream', '.png': 'image/png', '.svg': 'image/svg+xml',
};
const STREAM = 'Migrating the router now';
const TIMEOUT = 60000;

function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const port = s.address().port;
      s.close(() => resolve(port));
    });
  });
}

// The page's origin: the bundle under / and /app/, plus the portal routes
// answered from `state` (see the portal tests). `state.portal` is 'off'
// (503 on /api/me, the demo) or 'on'; the owner cookie gates the rest.
function startSite(port, state) {
  const base = `http://127.0.0.1:${port}`;
  const send = (res, code, body, headers = {}) => {
    res.writeHead(code, headers);
    res.end(body);
  };
  const json = (res, code, value) => send(res, code, JSON.stringify(value), { 'Content-Type': 'application/json' });
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => { body += c; });
    req.on('end', () => {
      const url = new URL(req.url, base);
      const owner = (req.headers.cookie || '').includes(`modron_owner=${COOKIE}`);
      const p = url.pathname;
      if (p.startsWith('/api/') || p.startsWith('/auth')) {
        if (p === '/api/me' && req.method === 'GET') {
          if (state.portal === 'off') return json(res, 503, { error: 'portal not configured' });
          return owner ? json(res, 200, { owner: true }) : json(res, 401, { error: 'unauthorized' });
        }
        if (p === '/auth' && req.method === 'POST') {
          if (state.rateLimited) return json(res, 429, { error: 'too many attempts' });
          const { token } = JSON.parse(body || '{}');
          if (token !== TOKEN) return json(res, 401, { error: 'unauthorized' });
          return send(res, 200, '{}', { 'Content-Type': 'application/json',
            'Set-Cookie': `modron_owner=${COOKIE}; Path=/; HttpOnly; SameSite=Strict` });
        }
        if (p === '/auth/logout' && req.method === 'POST') {
          state.loggedOut += 1;
          return send(res, 204, '', { 'Set-Cookie': 'modron_owner=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict' });
        }
        if (!owner) return json(res, 401, { error: 'unauthorized' });
        if (p === '/api/sessions' && req.method === 'GET') return json(res, 200, state.sessions);
        if (p === '/api/invites' && req.method === 'GET') {
          const sid = url.searchParams.get('session_id');
          return json(res, 200, state.invites.filter((i) => !i.revoked && i.session_id === sid));
        }
        if (p === '/api/invites' && req.method === 'POST') {
          const input = JSON.parse(body || '{}');
          state.invitePosts.push(input);
          const id = `inv${state.invitePosts.length}`;
          const row = { id, session_id: input.session_id, role: input.role, revoked: false,
            expires_at: '2026-11-16T10:00:00Z', url: `${base}/i/${id}.sig` };
          state.invites.push(row);
          return json(res, 201, { id, url: row.url, expires_at: row.expires_at });
        }
        const m = /^\/api\/invites\/([^/]+)$/.exec(p);
        if (m && req.method === 'DELETE') {
          const row = state.invites.find((i) => i.id === m[1]);
          if (row) row.revoked = true;
          state.deletes.push(m[1]);
          return send(res, 204, '');
        }
        return json(res, 404, { error: 'not found' });
      }
      // Static: the bundle at / and at /app/.
      let rel = decodeURIComponent(p);
      if (rel === '/app' || rel.startsWith('/app/')) rel = rel.slice(4) || '/';
      let file = path.join(DIST, rel);
      if (!file.startsWith(DIST)) return send(res, 403, 'forbidden');
      if (rel === '/' || (fs.existsSync(file) && fs.statSync(file).isDirectory())) file = path.join(file, 'index.html');
      fs.readFile(file, (err, data) => {
        if (err) return send(res, 404, 'not found');
        send(res, 200, data, { 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-cache' });
      });
    });
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => resolve({ base, state, close: () => server.close() }));
  });
}

// ---- minimal RFC 6455 server: handshake, unmasking, text frames, close ----
const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

// Parses one frame from buf; null when it is incomplete.
function parseFrame(buf) {
  if (buf.length < 2) return null;
  const opcode = buf[0] & 0x0f;
  const masked = (buf[1] & 0x80) !== 0;
  let len = buf[1] & 0x7f;
  let off = 2;
  if (len === 126) { if (buf.length < 4) return null; len = buf.readUInt16BE(2); off = 4; }
  else if (len === 127) { if (buf.length < 10) return null; len = Number(buf.readBigUInt64BE(2)); off = 10; }
  let mask = null;
  if (masked) { if (buf.length < off + 4) return null; mask = buf.subarray(off, off + 4); off += 4; }
  if (buf.length < off + len) return null;
  const payload = Buffer.from(buf.subarray(off, off + len));
  if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
  return { opcode, payload, size: off + len };
}

function textFrame(text) {
  const body = Buffer.from(text);
  const head = body.length < 126 ? Buffer.from([0x81, body.length])
    : Buffer.from([0x81, 126, body.length >> 8, body.length & 0xff]);
  return Buffer.concat([head, body]);
}

// Starts the scripted modron server. `submitted` resolves with the first
// submit frame the client sends. `opts.live` is the attach snapshot's live
// state (e.g. { running: true }); `opts.after` are frames sent after the
// attach snapshot (e.g. an approval request). `opts.shareLink` sends the
// snapshot and stream unprompted after the welcome, as a share link's
// server does (no attach). `frames` keeps every client frame, parsed.
function startWsServer(opts = {}) {
  const received = [];
  const frames = [];
  let onSubmit;
  const submitted = new Promise((resolve) => { onSubmit = resolve; });
  const server = http.createServer((req, res) => { res.writeHead(404); res.end(); });
  server.on('upgrade', (req, socket) => {
    const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key'] + GUID).digest('base64');
    socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`);
    const send = (frame) => socket.write(textFrame(JSON.stringify(frame)));
    let buf = Buffer.alloc(0);
    socket.on('error', () => {});
    socket.on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      for (let f = parseFrame(buf); f; f = parseFrame(buf)) {
        buf = buf.subarray(f.size);
        if (f.opcode === 8) { socket.end(); return; }
        if (f.opcode === 9) { socket.write(Buffer.from([0x8a, 0])); continue; }
        if (f.opcode !== 1) continue;
        const frame = JSON.parse(f.payload.toString('utf8'));
        received.push(frame.type);
        if (frame.type === 'hello') {
          send({ seq: 0, type: 'welcome', server_version: '0.1.3', protocol: 1 });
          if (opts.shareLink) {
            send({ session: SESSION, seq: 0, type: 'snapshot', session_id: SESSION, last_seq: 0, events: [], live: {} });
            send({ session: SESSION, seq: 1, type: 'delta', delta: { kind: 'text', text: STREAM } });
          }
        } else if (frame.type === 'attach') {
          send({ session: SESSION, seq: 0, type: 'snapshot', session_id: SESSION, last_seq: 0, events: [], live: opts.live || {} });
          send({ session: SESSION, seq: 1, type: 'delta', delta: { kind: 'text', text: STREAM } });
          (opts.after || []).forEach((extra, i) => send({ session: SESSION, seq: 2 + i, ...extra }));
        } else if (frame.type === 'submit') {
          onSubmit(frame);
        } else if (frame.type === 'router_defaults_req') {
          // The model picker's source: the router's defaults.
          send({ session: SESSION, seq: 4, type: 'router_defaults', authed: ['anthropic'], recommended_cheap: 'claude-haiku-5-5', recommended_strong: 'claude-opus-5', cheap_fallback: [], strong_fallback: ['claude-sonnet-5'] });
        } else if (frame.type === 'list_sessions') {
          // The sessions screen's refresh: answer with the scripted rows.
          send({ session: SESSION, seq: 3, type: 'sessions', sessions: opts.sessions || [] });
        }
        frames.push(frame);
      }
    });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => {
    resolve({ url: `ws://127.0.0.1:${server.address().port}/`, submitted, received, frames, close: () => server.close() });
  }));
}

// Page errors and console errors both count: a logged wasm panic is a failure
// even when the page keeps running.
// The browser logs every failed HTTP answer as a console error. The portal's
// probes answer 401, 429 or 503 on purpose, so those log lines are allowed;
// any other error, and any page error, still fails a case.
const EXPECTED_STATUS = /Failed to load resource: the server responded with a status of (401|429|503)\b/;
function watchErrors(page, errors) {
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => {
    if (m.type() === 'error' && !EXPECTED_STATUS.test(m.text())) errors.push('console: ' + m.text());
  });
}

async function openPage(browser, url) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 800 } });
  const errors = [];
  watchErrors(page, errors);
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  return { page, errors };
}

// A page in its own browser context (its own cookies), for the portal cases.
async function openIsolated(browser, url, cookie) {
  const context = await browser.newContext({ viewport: { width: 1000, height: 800 } });
  if (cookie) await context.addCookies([{ name: 'modron_owner', value: COOKIE, url: cookie }]);
  const page = await context.newPage();
  const errors = [];
  watchErrors(page, errors);
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  return { context, page, errors };
}

// The page's storage and address must never hold the token or the cookie.
async function assertNoSecrets(page) {
  const seen = await page.evaluate(() => [location.href, JSON.stringify(localStorage), JSON.stringify(sessionStorage)].join('\n'));
  for (const secret of [TOKEN, COOKIE]) {
    if (seen.includes(secret)) throw new Error('secret found in the page address or storage');
  }
}

const PORTAL_ROWS = [
  { id: 'aaaa', title: 'fix router', host_label: 'laptop', host_kind: 'local', viewer_key: 'vk1', state: 'idle',
    online: true, updated_at: '2026-11-15T10:20:30Z', open_url: '/app/?opened=fix' },
  { id: 'bbbb', title: 'old docs', host_label: 'cloud', host_kind: 'cloud', viewer_key: 'vk2', state: 'offline',
    online: false, updated_at: '2026-11-14T08:00:00Z', open_url: '/app/?opened=docs' },
];

// Sign-in, wrong token then right token; the input is cleared after each try.
async function testPortalSignIn(browser, site) {
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/');
  try {
    await page.waitForSelector('input[type="password"]', { timeout: TIMEOUT });
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('wrong-token');
    await page.waitForFunction(() => document.activeElement.value === 'wrong-token', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('wrong token'), null, { timeout: TIMEOUT });
    await page.waitForFunction(() => document.activeElement && document.activeElement.value === '', null, { timeout: TIMEOUT });
    await page.keyboard.type(TOKEN);
    await page.waitForFunction((t) => document.activeElement.value === t, TOKEN, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'fix router', { timeout: TIMEOUT });
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// An online row opens its URL; an offline row does not.
async function testPortalRows(browser, site) {
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'fix router', { timeout: TIMEOUT });
    await page.getByText('old docs', { exact: false }).first().click();
    await page.waitForTimeout(800);
    if (page.url().includes('opened=docs')) throw new Error('an offline row opened its URL');
    await page.getByText('fix router', { exact: false }).first().click();
    await page.waitForURL(/opened=fix/, { timeout: TIMEOUT });
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// Sign out goes back to the sign-in screen and asks the server to log out.
async function testPortalSignOut(browser, site) {
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'sign out', { timeout: TIMEOUT });
    await tapLabel(page, 'sign out');
    await page.waitForSelector('input[type="password"]', { timeout: TIMEOUT });
    await page.waitForFunction(() => document.body.innerText.includes('owner token') || document.querySelector('input[type="password"]'), null, { timeout: TIMEOUT });
    if (site.state.loggedOut !== 1) throw new Error('logout requests: ' + site.state.loggedOut);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// Settings share: create a viewer invite for the attached session, show its
// URL, list it, then revoke it.
async function testInviteFromSettings(browser, site) {
  const ws = await startWsServer();
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/?url=' + encodeURIComponent(ws.url) + '&session=' + SESSION, site.base);
  try {
    await page.waitForFunction((s) => document.body.innerText.includes(s), STREAM, { timeout: TIMEOUT });
    await tapLabel(page, 'settings');
    await page.waitForFunction(() => document.body.innerText.includes('share'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'viewer');
    await page.waitForFunction(() => document.body.innerText.includes('/i/inv1.sig'), null, { timeout: TIMEOUT });
    const post = site.state.invitePosts[0];
    if (!post || post.session_id !== SESSION || post.role !== 'viewer' || post.ttl_hours !== 24) {
      throw new Error('invite request was ' + JSON.stringify(post));
    }
    await page.waitForFunction(() => document.body.innerText.includes('viewer \u00b7 expires'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'copy');
    await tapLabel(page, 'revoke');
    await page.waitForFunction(() => document.body.innerText.includes('no invites'), null, { timeout: TIMEOUT });
    if (site.state.deletes[0] !== 'inv1') throw new Error('revoke deleted ' + site.state.deletes.join(','));
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

function newSiteState(portal) {
  return { portal, rateLimited: false, sessions: PORTAL_ROWS, invites: [], invitePosts: [], deletes: [], loggedOut: 0 };
}

async function testDemo(browser, base) {
  const { page, errors } = await openPage(browser, base + '/');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  const text = await page.evaluate(() => document.body.innerText);
  if (!/claude-opus-5 · 0 tok/.test(text)) throw new Error('status line missing:\n' + text);
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}

async function testLive(browser, base, wsUrl, ws) {
  const query = `?url=${encodeURIComponent(wsUrl)}&session=${SESSION}`;
  const { page, errors } = await openPage(browser, base + '/' + query);
  await page.waitForFunction((s) => document.body.innerText.includes(s), STREAM, { timeout: TIMEOUT });
  const text = await page.evaluate(() => document.body.innerText);
  if (!text.includes('connected')) throw new Error('connection label missing:\n' + text);
  // The composer is an <input>; wait for it to take focus and for the typed
  // text to land before Enter, so no key is lost to a page still settling.
  await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
  await page.keyboard.type('hi');
  await page.waitForFunction(() => document.activeElement.value === 'hi', null, { timeout: TIMEOUT });
  // NSH applies the change on its next engine turn; Enter must come after it.
  await page.waitForTimeout(300);
  await page.keyboard.press('Enter');
  // Let the submit (if any) cross the link before reporting.
  await page.waitForTimeout(1500);
  if (process.env.WEB_TEST_DEBUG) {
    const state = await page.evaluate(() => [document.activeElement && document.activeElement.value, document.body.innerText.slice(-200)]);
    console.log('debug: composer and page text after Enter: ' + JSON.stringify(state));
  }
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}

// Waits until the scripted server has received a client frame of `type`
// that passes `pred`; returns it or throws after 5 s.
async function waitFrame(ws, type, pred = () => true) {
  const deadline = Date.now() + 5000;
  for (;;) {
    const hit = ws.frames.find((f) => f.type === type && pred(f));
    if (hit) return hit;
    if (Date.now() > deadline) throw new Error(`no ${type} frame; server received: ${ws.received.join(',')}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

// Opens the live page against `ws` and waits until the stream text shows.
async function openLive(browser, base, ws) {
  const query = `?url=${encodeURIComponent(ws.url)}&session=${SESSION}`;
  const opened = await openPage(browser, base + '/' + query);
  await opened.page.waitForFunction((s) => document.body.innerText.includes(s), STREAM, { timeout: TIMEOUT });
  return opened;
}

// Clicks the page element whose whole text is `label`.
async function tapLabel(page, label) {
  await page.getByText(label, { exact: true }).first().click();
}

const APPROVAL = {
  type: 'approval_requested',
  request_id: 'req-1',
  call: { id: 'c1', name: 'edit', arguments: { path: 'a.rs' } },
};

// Stop control: visible while running; a tap sends an interrupt frame.
async function testStopTap(browser, base) {
  const ws = await startWsServer({ live: { running: true } });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('stop'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'stop');
    await waitFrame(ws, 'interrupt');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Esc on the web sends an interrupt, the terminal's key.
async function testEscapeInterrupts(browser, base) {
  const ws = await startWsServer({ live: { running: true } });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('stop'), null, { timeout: TIMEOUT });
    await page.keyboard.press('Escape');
    await waitFrame(ws, 'interrupt');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// A pending approval shows its three options as buttons; "allow once"
// sends the approve frame with the core's decision name.
async function testApprovalTap(browser, base) {
  const ws = await startWsServer({ after: [APPROVAL] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('allow once'), null, { timeout: TIMEOUT });
    const text = await page.evaluate(() => document.body.innerText);
    for (const label of ['allow all', 'deny']) {
      if (!text.includes(label)) throw new Error(`approval option missing: ${label}`);
    }
    await tapLabel(page, 'allow once');
    const frame = await waitFrame(ws, 'approve');
    if (frame.request_id !== 'req-1') throw new Error('approve frame names ' + frame.request_id);
    if (frame.decision !== 'allow_once') throw new Error('decision was ' + frame.decision);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// The theme reaches the web: the task row's text takes the oculus primary
// (#D9A520 = rgb(217, 165, 32)), read from the computed style.
async function testThemeColour(browser, base) {
  const { page, errors } = await openPage(browser, base + '/');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  const colour = await page.evaluate(() => {
    const leaf = [...document.querySelectorAll('*')].find(
      (e) => e.childElementCount === 0 && e.textContent.includes('fix the router crate'));
    return leaf ? getComputedStyle(leaf).color : null;
  });
  if (colour !== 'rgb(217, 165, 32)') throw new Error('task row colour is ' + colour + ', want the oculus primary');
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}

// A page opened with `?url=` only (a share link: no session). The server
// names session S in a snapshot; a typed message must be a submit for S,
// and the page must not send an attach.
async function testShareLinkSubmit(browser, base) {
  const ws = await startWsServer({ shareLink: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1000, height: 800 } });
    const errors = [];
    watchErrors(page, errors);
    await page.goto(base + '/?url=' + encodeURIComponent(ws.url), { waitUntil: 'domcontentloaded' });
    await page.waitForFunction((s) => document.body.innerText.includes(s), STREAM, { timeout: TIMEOUT });
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('hi');
    await page.waitForFunction(() => document.activeElement.value === 'hi', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    const frame = await waitFrame(ws, 'submit', (f) => f.task === 'hi');
    if (frame.session_id !== SESSION) throw new Error('submit carried ' + frame.session_id + ', want the adopted session');
    if (ws.received.includes('attach')) throw new Error('page sent an attach on a share link');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Shift+Enter adds a line without submitting; a later Enter submits both lines.
async function testShiftEnterNewline(browser, base) {
  const ws = await startWsServer({});
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.keyboard.type('hi');
    await page.waitForFunction(() => document.activeElement.value === 'hi', null, { timeout: TIMEOUT });
    await page.keyboard.down('Shift');
    await page.keyboard.press('Enter');
    await page.keyboard.up('Shift');
    await page.keyboard.type('there');
    await page.waitForFunction(() => document.activeElement.value.includes('there'), null, { timeout: TIMEOUT });
    await page.waitForTimeout(400);
    if (ws.frames.some((f) => f.type === 'submit')) throw new Error('shift+enter submitted');
    await page.keyboard.press('Enter');
    const frame = await waitFrame(ws, 'submit', (f) => f.task === 'hi\nthere');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
    return frame;
  } finally {
    ws.close();
  }
}

// Esc on a pending approval denies it (the third decision), it does not interrupt.
async function testEscDeniesApproval(browser, base) {
  const ws = await startWsServer({ after: [APPROVAL] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('allow once'), null, { timeout: TIMEOUT });
    await page.keyboard.press('Escape');
    const frame = await waitFrame(ws, 'approve');
    if (frame.decision !== 'deny') throw new Error('esc decision was ' + frame.decision);
    if (ws.frames.some((f) => f.type === 'interrupt')) throw new Error('esc interrupted instead of denying');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// A queue chip is a button: pressing "[2] two" sends dequeue for index 1.
async function testChipDequeue(browser, base) {
  const ws = await startWsServer({ after: [{ type: 'queue_changed', queued: ['one', 'two'] }] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('[2] two'), null, { timeout: TIMEOUT });
    await tapLabel(page, '[2] two');
    const frame = await waitFrame(ws, 'dequeue');
    if (frame.index !== 1) throw new Error('dequeue index was ' + frame.index + ', want 1');
    if (frame.session_id !== SESSION) throw new Error('dequeue carried the wrong session');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// The header's pressable controls open the screens (demo: no server).
async function testHeaderOpensScreens(browser, base) {
  const { page, errors } = await openPage(browser, base + '/');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  await tapLabel(page, 'sessions');
  await page.waitForFunction(() => document.body.innerText.includes('write the release notes'), null, { timeout: TIMEOUT });
  await tapLabel(page, 'settings');
  await page.waitForFunction(() => document.body.innerText.includes('approval mode: ask'), null, { timeout: TIMEOUT });
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}
// A session row is pressable: a tap sends an attach frame for that session.
async function testSessionRowAttaches(browser, base) {
  const other = '9c2d7e10-1a2b-4c3d-8e4f-5a6b7c8d9e0f';
  const ws = await startWsServer({ sessions: [{ id: other, title: 'other work', created_at: '', updated_at: '', message_count: 2 }] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await tapLabel(page, 'sessions');
    await page.waitForFunction(() => document.body.innerText.includes('other work'), null, { timeout: TIMEOUT });
    await page.getByText('other work', { exact: false }).first().click();
    const frame = await waitFrame(ws, 'attach', (f) => f.session_id === other);
    if (frame.session_id !== other) throw new Error('attach carried ' + frame.session_id);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}
// The effort row is pressable: a tap on "effort: unset" sends set_config
// with effort low (unset -> low).
async function testEffortTapSendsConfig(browser, base) {
  const ws = await startWsServer();
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await tapLabel(page, 'settings');
    await page.waitForFunction(() => document.body.innerText.includes('effort: unset'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'effort: unset');
    const frame = await waitFrame(ws, 'set_config', (f) => f.patch && f.patch.effort === 'low');
    if (frame.patch.effort !== 'low') throw new Error('effort patch was ' + JSON.stringify(frame.patch));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Packet D1. Ctrl+T with no tool activity shows the note and opens nothing;
// "!ls" sends a shell frame and never a submit.
async function testCtrlTAndShellPassthrough(browser, base) {
  const ws = await startWsServer();
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.keyboard.press('Control+t');
    await page.waitForFunction(() => document.body.innerText.includes('no tool activity yet'), null, { timeout: TIMEOUT });
    await page.keyboard.type('!ls');
    await page.keyboard.press('Enter');
    const frame = await waitFrame(ws, 'shell', (f) => f.command === 'ls');
    if (frame.command !== 'ls') throw new Error('shell command was ' + frame.command);
    if (ws.received.includes('submit')) throw new Error('a shell line was submitted: ' + ws.received.join(','));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Packet C3f. Typing "/co" in the composer shows the compact suggestion.
async function testSlashSuggestions(browser, base) {
  const { page, errors } = await openPage(browser, base + '/');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
  await page.keyboard.type('/co');
  await page.waitForFunction(() => document.body.innerText.includes("/compact - compact this session"), null, { timeout: TIMEOUT });
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}

// The model picker: /model opens it, a tap on a row sends set_config with that model.
async function testModelPickerSendsConfig(browser, base) {
  const ws = await startWsServer();
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/model');
    await page.waitForFunction(() => document.activeElement.value === '/model', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('claude-haiku-5-5 (recommended cheap)'), null, { timeout: TIMEOUT });
    await page.getByText('claude-haiku-5-5', { exact: false }).first().click();
    const frame = await waitFrame(ws, 'set_config', (f) => f.patch && f.patch.model === 'claude-haiku-5-5');
    if (frame.patch.model !== 'claude-haiku-5-5') throw new Error('model patch was ' + JSON.stringify(frame.patch));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Delete asks inline; the yes control sends delete_session for that row.
async function testDeleteAfterConfirmation(browser, base) {
  const other = '9c2d7e10-1a2b-4c3d-8e4f-5a6b7c8d9e0f';
  const ws = await startWsServer({ sessions: [{ id: other, title: 'other work', created_at: '', updated_at: '', message_count: 2 }] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await tapLabel(page, 'sessions');
    await page.waitForFunction(() => document.body.innerText.includes('other work'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'delete');
    await page.waitForFunction(() => document.body.innerText.includes('delete other work? y / n'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    const frame = await waitFrame(ws, 'delete_session', (f) => f.session_id === other);
    if (frame.session_id !== other) throw new Error('delete carried ' + frame.session_id);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// The agent screen: a tap on the sub-agent row opens it; Enter steers; kill asks, then y.
async function testAgentSteerAndKill(browser, base) {
  const agent = { id: 'agent-review', label: 'reviewer', turns: 2, tokens: 900, last_tool: 'read', done: null, state: 'running' };
  const ws = await startWsServer({ live: { running: true, subagents: [agent] } });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('[reviewer] running'), null, { timeout: TIMEOUT });
    await page.getByText('[reviewer] running', { exact: false }).first().click();
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('focus tests');
    await page.waitForFunction(() => document.activeElement.value === 'focus tests', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    const steer = await waitFrame(ws, 'steer_agent', (f) => f.text === 'focus tests');
    if (steer.agent !== 'agent-review') throw new Error('steer carried ' + steer.agent);
    await tapLabel(page, 'kill');
    await page.waitForFunction(() => document.body.innerText.includes('kill reviewer? y / n'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    const kill = await waitFrame(ws, 'kill_agent', (f) => f.agent === 'agent-review');
    if (kill.agent !== 'agent-review') throw new Error('kill carried ' + kill.agent);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

(async () => {
  const sitePort = await freePort();
  // The portal is off here (/api/me 503), so no-url pages are the demo.
  const site = await startSite(sitePort, newSiteState('off'));
  const base = site.base;
  const ws = await startWsServer();
  const browser = await chromium.launch({ args: ['--no-sandbox'] });
  let failed = 0;
  const portalSite = async (portal) => startSite(await freePort(), newSiteState(portal));
  try {
    await testDemo(browser, base);
    console.log('PASS (a) demo mode: transcript row and status line are DOM text');
  } catch (e) {
    failed++;
    console.log('FAIL (a) demo mode: ' + e.message);
  }
  // Stop, Esc, approval and theme: they run on the same browser.
  const more = [
    ['(c) stop control sends interrupt', () => testStopTap(browser, base)],
    ['(d) Esc on web sends interrupt', () => testEscapeInterrupts(browser, base)],
    ['(e) approval buttons send approve', () => testApprovalTap(browser, base)],
    ['(f) theme primary colour applied on web', () => testThemeColour(browser, base)],
    ['(g) share link (no session in URL): submit is for the adopted session', () => testShareLinkSubmit(browser, base)],
    ['(h) queue chip press sends dequeue', () => testChipDequeue(browser, base)],
    ['(i) header controls open the sessions and settings screens', () => testHeaderOpensScreens(browser, base)],
    ['(j) session row tap sends attach', () => testSessionRowAttaches(browser, base)],
    ['(k) effort tap sends set_config', () => testEffortTapSendsConfig(browser, base)],
    ['(l) typing /co shows the compact suggestion', () => testSlashSuggestions(browser, base)],
    ['(m) model picker row sends set_config with the model', () => testModelPickerSendsConfig(browser, base)],
    ['(n) delete asks inline, then the yes control sends delete_session', () => testDeleteAfterConfirmation(browser, base)],
    ['(o) agent screen: steer sends steer_agent, kill confirms then sends kill_agent', () => testAgentSteerAndKill(browser, base)],
    ['(q) shift+enter adds a line and does not submit', () => testShiftEnterNewline(browser, base)],
    ['(r) esc on a pending approval sends deny', () => testEscDeniesApproval(browser, base)],
  ];
  // The portal cases: each on its own site (its own state) and context.
  const portal = [
    ['(p1) no url, portal on: sign-in, wrong token then right token shows the list', async () => {
      const s = await portalSite('on'); try { await testPortalSignIn(browser, s); } finally { s.close(); } }],
    ['(p2) portal list: an online row opens its URL, an offline row does not', async () => {
      const s = await portalSite('on'); try { await testPortalRows(browser, s); } finally { s.close(); } }],
    ['(p3) portal sign out returns to the sign-in screen', async () => {
      const s = await portalSite('on'); try { await testPortalSignOut(browser, s); } finally { s.close(); } }],
    ['(p4) settings share: a viewer invite is created, shown and revoked', async () => {
      const s = await portalSite('on'); try { await testInviteFromSettings(browser, s); } finally { s.close(); } }],
    ['(d1) ctrl+t note, and "!ls" sends a shell frame (no submit)', () => testCtrlTAndShellPassthrough(browser, base)],
  ];
  more.push(...portal);
  for (const [name, run] of more) {
    try {
      await run();
      console.log('PASS ' + name);
    } catch (e) {
      failed++;
      console.log('FAIL ' + name + ': ' + e.message);
    }
  }
  try {
    await testLive(browser, base, ws.url, ws);
    const frame = await Promise.race([
      ws.submitted,
      new Promise((_, reject) => setTimeout(() => reject(new Error('no submit frame; server received: ' + ws.received.join(','))), 5000)),
    ]);
    if (frame.task !== 'hi') throw new Error(`submit task was ${JSON.stringify(frame.task)}`);
    if (frame.session_id !== SESSION) throw new Error('submit carried the wrong session');
    console.log('PASS (b) live mode: streamed text shown; typed "hi" + Enter sent a submit frame');
  } catch (e) {
    failed++;
    console.log('FAIL (b) live mode: ' + e.message);
  } finally {
    await browser.close();
    ws.close();
    site.close();
  }
  process.exit(failed ? 1 : 0);
})();
