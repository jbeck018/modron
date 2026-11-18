// Browser tests for the web host (run from app/ after `nsh package --target web -o dist`).
//   (a) demo mode: a transcript row and the status line show as DOM text.
//   (b) live mode: a scripted WebSocket server (below, RFC 6455 by hand) sends
//       welcome, a snapshot and a streamed delta; typing + Enter sends a submit.
//   (p1..p5) owner portal: sign-in, the session list, open and offline rows,
//       sign-out, and an invite created, shown and revoked from Settings.
//   (w1..w6) Settings "notify this device": hidden on 503, on/off from the
//       browser's subscription, a tap subscribes (exact body), a denied
//       permission, a second tap unsubscribes, a viewer invite hides it,
//       an approver invite subscribes with its invite, iOS tab hint. The push
//       service is stubbed in the page (no real push service is reached).
//   (c1..c7) Cloud screen (PLAN 10.10) on a scripted bridge: the entry hidden
//       on 503, a masked token POSTed and listed, a refused token showing
//       GitHub's status, remove asks first, the repo picker's query and paging,
//       a run started with {account, repo, branch}, stop / wake / open routes,
//       one shared line when runs are not configured.
//   (u1) sign-in card (UX-1): a 44 px sign-in button, no sideways scroll.
//   (v1) vault (PLAN 10.9 item 3): the credentials rows read the vault, a key
//       is PUT over HTTPS (never a session frame), removal DELETEs.
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
const assert = require('assert');

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
        // Web push: the owner, or an invite the page carries (approver ok,
        // viewer 403), as cloud/bridge/src/index.ts pushGrant does.
        if (p === '/api/push/key' || p === '/api/push/subscribe') {
          if (state.push === 'off') return json(res, 503, { error: 'web push is not configured' });
          const parsed = JSON.parse(body || '{}');
          const invite = p === '/api/push/key' ? url.searchParams.get('invite') : (parsed.invite ?? null);
          if (p === '/api/push/key') state.pushKeyRequests.push(invite);
          const grant = owner ? 'owner' : invite === 'approver-token' ? 'approver' : invite === 'viewer-token' ? 'viewer' : null;
          if (grant === null) return json(res, 401, { error: 'unauthorized' });
          if (grant === 'viewer') return json(res, 403, { error: 'only owners and approver invites may subscribe' });
          if (p === '/api/push/key') return json(res, 200, { key: PUSH_KEY });
          if (req.method === 'POST') { state.pushPosts.push(parsed); return send(res, 204, ''); }
          if (req.method === 'DELETE') { state.pushDeletes.push(parsed); return send(res, 204, ''); }
          return json(res, 405, { error: 'method not allowed' });
        }
        if (!owner) return json(res, 401, { error: 'unauthorized' });
        // Cloud (PLAN 10.10): linked GitHub accounts, the repo picker, runs.
        // The vault (PLAN 10.9): the credentials list, a key PUT and a DELETE.
        if (p === '/api/github/accounts' && req.method === 'GET') {
          state.accountGets += 1;
          if (state.cloud === 'off') return json(res, 503, { error: 'vault not configured' });
          return json(res, 200, { accounts: state.accounts });
        }
        if (p === '/api/github/accounts' && req.method === 'POST') {
          const input = JSON.parse(body || '{}');
          state.accountPosts.push(input);
          if (state.refusedTokens.includes(input.token)) {
            return json(res, 400, { error: 'GitHub refused the token', github_status: 401 });
          }
          const row = { id: `a${state.accounts.length + 1}`, login: 'octocat', kind: 'token' };
          state.accounts.push(row);
          return json(res, 201, row);
        }
        const account = /^\/api\/github\/accounts\/([^/]+)$/.exec(p);
        if (account && req.method === 'DELETE') {
          state.accounts = state.accounts.filter((a) => a.id !== account[1]);
          state.accountDeletes.push(account[1]);
          return send(res, 204, '');
        }
        if (p === '/api/github/repos' && req.method === 'GET') {
          const q = url.searchParams.get('q') || '';
          const page = Number(url.searchParams.get('page') || '1');
          state.repoQueries.push({ q, page, account: url.searchParams.get('account') });
          const all = state.repos.filter((r) => r.full_name.includes(q));
          const repos = all.slice((page - 1) * REPO_PAGE, page * REPO_PAGE);
          return json(res, 200, { repos, next_page: page * REPO_PAGE < all.length ? page + 1 : null });
        }
        if (p === '/api/runs' && req.method === 'GET') {
          if (state.runsOff) return json(res, 503, { error: 'cloud runs are not configured' });
          return json(res, 200, { runs: state.runs });
        }
        if (p === '/api/runs' && req.method === 'POST') {
          if (state.runsOff) return json(res, 503, { error: 'cloud runs are not configured' });
          const input = JSON.parse(body || '{}');
          state.runPosts.push(input);
          const row = { id: `r${state.runs.length + 1}`, repo: input.repo, account: input.account,
            branch: input.branch, state: 'starting', reason: '', created_at: '2026-11-16T09:00:00Z', session_url: '' };
          state.runs.push(row);
          return json(res, 201, row);
        }
        const run = /^\/api\/runs\/([^/]+)\/(stop|wake)$/.exec(p);
        if (run && req.method === 'POST') {
          const row = state.runs.find((r) => r.id === run[1]);
          state.runActions.push({ id: run[1], action: run[2] });
          if (row) row.state = run[2] === 'stop' ? 'stopped' : 'running';
          return send(res, 204, '');
        }
        if (p === '/api/credentials' && req.method === 'GET') {
          if (state.vault === 'off') return json(res, 503, { error: 'vault not configured' });
          return json(res, 200, { credentials: state.credentials });
        }
        const credential = /^\/api\/credentials\/([^/]+)$/.exec(p);
        if (credential && req.method === 'PUT') {
          const input = JSON.parse(body || '{}');
          state.vaultPuts.push({ id: credential[1], body: input });
          const row = state.credentials.find((c) => c.id === credential[1]);
          if (row) row.set = true;
          else state.credentials.push({ id: credential[1], set: true });
          return send(res, 204, '');
        }
        if (credential && req.method === 'DELETE') {
          state.vaultDeletes.push(credential[1]);
          const row = state.credentials.find((c) => c.id === credential[1]);
          if (row) row.set = false;
          return send(res, 204, '');
        }
        if (p === '/api/sessions' && req.method === 'GET') return json(res, 200, state.sessions);
        if (p === '/api/invites' && req.method === 'GET') {
          const sid = url.searchParams.get('session_id');
          return json(res, 200, state.invites.filter((i) => !i.revoked && i.session_id === sid));
        }
        if (p === '/api/invites' && req.method === 'POST') {
          const input = JSON.parse(body || '{}');
          state.invitePosts.push(input);
          const id = String(state.invitePosts.length).padStart(32, '0');
          const row = { id, session_id: input.session_id, role: input.role, revoked: false,
            expires_at: '2026-11-16T10:00:00Z', url: `${base}/i/${id}.${'ab'.repeat(32)}` };
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
  let dropped = false;
  // refuseAfterDrop: once dropped, reconnects get a 503 until `lift()`.
  let refusing = true;
  const usedTickets = new Set();
  const received = [];
  const frames = [];
  let onSubmit;
  const submitted = new Promise((resolve) => { onSubmit = resolve; });
  const server = http.createServer((req, res) => { res.writeHead(404); res.end(); });
  server.on('upgrade', (req, socket) => {
    const ticket = new URL(req.url, 'http://ws.test').searchParams.get('ticket');
    if (ticket) {
      if (usedTickets.has(ticket)) { socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'); return; }
      usedTickets.add(ticket);
    }
    if (opts.refuseAfterDrop && dropped && refusing) {
      socket.end('HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n');
      return;
    }
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
          send({ seq: 0, type: 'welcome', server_version: '0.1.3', protocol: 1, commands: opts.commands || [] });
          if (opts.shareLink) {
            send({ session: SESSION, seq: 0, type: 'snapshot', session_id: SESSION, last_seq: 0, events: [], live: {} });
            send({ session: SESSION, seq: 1, type: 'delta', delta: { kind: 'text', text: STREAM } });
          }
        } else if (frame.type === 'attach') {
          // Forced drop (opts.dropOnce): the page must reconnect and re-attach.
          if (opts.dropOnce && !dropped) {
            dropped = true;
            setTimeout(() => socket.destroy(), 1500);
          }
          send({ session: SESSION, seq: 0, type: 'snapshot', session_id: SESSION, last_seq: 0, events: opts.events || [], live: opts.live || {} });
          // quiet: no streamed text, so the session stays idle (attachments need an idle session).
          if (!opts.quiet) send({ session: SESSION, seq: 1, type: 'delta', delta: { kind: 'text', text: STREAM } });
          (opts.after || []).forEach((extra, i) => send({ session: SESSION, seq: 2 + i, ...extra }));
        } else if (frame.type === 'submit') {
          onSubmit(frame);
          // reasoningOnSubmit: the reply streams its reasoning (the page shows it
          // only after the user opted in with /reasoning).
          if (opts.reasoningOnSubmit) {
            send({ session: SESSION, seq: 30, type: 'delta', delta: { kind: 'reasoning', text: 'The router keeps two tables, so the matcher is the piece to split out first.\n' } });
            send({ session: SESSION, seq: 31, type: 'delta', delta: { kind: 'reasoning', text: 'The handlers then follow the same path into the new crate.' } });
          }
        } else if (frame.type === 'steer') {
          // The scripted stream leaves the session running, so the composer
          // steers (PROTOCOL_V2 section 2). The cases below only care that
          // the typed text reaches this session: record the steer in the
          // submit shape they read.
          const delivered = { type: 'submit', task: frame.text, session_id: frame.session_id, steer: true };
          frames.push(delivered);
          onSubmit(delivered);
        } else if (frame.type === 'router_defaults_req') {
          // The model picker's source: the router's defaults.
          send({ session: SESSION, seq: 4, type: 'router_defaults', authed: ['anthropic'], recommended_cheap: 'claude-haiku-5-5', recommended_strong: 'claude-opus-5', cheap_fallback: [], strong_fallback: ['claude-sonnet-5'] });
        } else if (frame.type === 'auth_providers_req') {
          // The credentials screen's rows (the first-run check reads them too).
          send({ session: SESSION, seq: 5, type: 'auth_providers', rows: opts.providers || [] });
        } else if (frame.type === 'personas_req') {
          // The agent picker's rows.
          send({ session: SESSION, seq: 6, type: 'personas', personas: opts.personas || [] });
        } else if (frame.type === 'auth_start' && opts.oauth) {
          // The device flow (ChatGPT, Claude): progress with the URL and the
          // one-time code, then the outcome as a note. No token leaves here.
          send({ session: SESSION, seq: 7, type: 'o_auth_progress', provider: frame.provider, stage: 'waiting', message: 'enter the code in the browser window', url: 'https://auth.openai.com/codex/device', user_code: 'WXYZ-1234' });
          // The outcome: a `done` progress (shown on this screen) and a note (the transcript).
          setTimeout(() => {
            send({ session: SESSION, seq: 8, type: 'o_auth_progress', provider: frame.provider, stage: 'done', message: 'chatgpt signed in (sk-...ab12)' });
            send({ session: SESSION, seq: 9, type: 'note', text: 'openai-compat: chatgpt signed in (sk-...ab12)' });
          }, 300);
        } else if (frame.type === 'list_sessions') {
          // The sessions screen's refresh: answer with the scripted rows.
          send({ session: SESSION, seq: 3, type: 'sessions', sessions: opts.sessions || [] });
        }
        frames.push(frame);
      }
    });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => {
    resolve({ url: `ws://127.0.0.1:${server.address().port}/`, submitted, received, frames, close: () => server.close(), lift: () => { refusing = false; } });
  }));
}

// Page errors and console errors both count: a logged wasm panic is a failure
// even when the page keeps running.
// The browser logs every failed HTTP answer as a console error. The portal's
// probes answer 401, 429 or 503 on purpose, so those log lines are allowed;
// any other error, and any page error, still fails a case.
// 400 is the bridge's answer to a refused GitHub token (the page shows its note).
const EXPECTED_STATUS = /Failed to load resource: the server responded with a status of (400|401|429|503)\b/;
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
    // The whole link must be one run of text (a wrap splits it, and the e2e
    // harness reads the link from the page text): 64 hex signature digits.
    await page.waitForFunction(() => /\/i\/0{31}1\.[0-9a-f]{64}/.test(document.body.innerText), null, { timeout: TIMEOUT });
    const shown = await page.evaluate(() => (document.body.innerText.match(/https?:\/\/\S+\/i\/\S+/) || [''])[0]);
    const issued = site.state.invites[0] && site.state.invites[0].url;
    if (!issued || shown !== issued) throw new Error(`the page shows ${shown}, the bridge issued ${issued}`);
    const post = site.state.invitePosts[0];
    if (!post || post.session_id !== SESSION || post.role !== 'viewer' || post.ttl_hours !== 24) {
      throw new Error('invite request was ' + JSON.stringify(post));
    }
    await page.waitForFunction(() => document.body.innerText.includes('viewer \u00b7 expires'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'copy');
    await tapLabel(page, 'revoke');
    await page.waitForFunction(() => document.body.innerText.includes('no invites'), null, { timeout: TIMEOUT });
    if (site.state.deletes[0] !== '0'.repeat(31) + '1') throw new Error('revoke deleted ' + site.state.deletes.join(','));
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// The sign-in card (UX-1): the sign-in control is a real button at least 44 px
// tall, and the page does not scroll sideways.
async function testSignInLayout(browser, site) {
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/');
  try {
    await page.waitForSelector('input[type="password"]', { timeout: TIMEOUT });
    const box = await page.getByRole('button', { name: 'sign in' }).first().boundingBox();
    if (!box || box.height < 44) throw new Error('sign-in button is ' + (box ? box.height : 'missing') + ' px tall');
    const sideways = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
    if (sideways) throw new Error('the sign-in page scrolls sideways');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// One page load opens one socket. A second socket for the same load (a
// reconnect that reuses a single-use ticket is refused by the bridge) fails
// this: the count of `websocket` events must be exactly 1 after the page has
// had time to connect, for a live URL, an invite ticket and an owner page.
async function testOneSocketPerLoad(browser, site) {
  const ws = await startWsServer();
  try {
  const cases = [
    ['live', '/app/?url=' + encodeURIComponent(ws.url + '/ws/k1') + '&session=' + SESSION, null],
    ['invite ticket', '/app/?url=' + encodeURIComponent(ws.url + '/ws/k2?ticket=t2') + '&invite=x.y', null],
    ['owner', '/app/?url=' + encodeURIComponent(ws.url + '/ws/k3') + '&session=' + SESSION, site.base],
  ];
  for (const [label, path, cookie] of cases) {
    // The listeners are set before the first navigation, so no early socket is missed.
    const context = await browser.newContext({ viewport: { width: 1000, height: 800 } });
    if (cookie) await context.addCookies([{ name: 'modron_owner', value: COOKIE, url: cookie }]);
    try {
      const page = await context.newPage();
      const errors = [];
      watchErrors(page, errors);
      const sockets = [];
      let loads = 0;
      page.on('websocket', (w) => sockets.push(w.url()));
      page.on('load', () => { loads += 1; });
      await page.goto(site.base + path, { waitUntil: 'domcontentloaded' });
      await page.waitForFunction(() => document.body.innerText.includes('connected'), null, { timeout: TIMEOUT });
      await page.waitForTimeout(4000);
      if (loads !== 1) throw new Error(`${label}: the page loaded ${loads} times (a reload opens a second socket)`);
      if (sockets.length !== 1) throw new Error(`${label}: ${sockets.length} sockets for one page load`);
      // In-app navigation (chat, settings, back) must not reconnect.
      await tapLabel(page, 'settings');
      await page.waitForFunction(() => document.body.innerText.includes('approval mode'), null, { timeout: TIMEOUT });
      await page.keyboard.press('Escape');
      await page.waitForFunction(() => document.body.innerText.includes('connected'), null, { timeout: TIMEOUT });
      await page.waitForTimeout(2000);
      if (sockets.length !== 1) throw new Error(`${label}: ${sockets.length} sockets after chat -> settings -> back`);
      if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    } finally {
      await context.close();
    }
  }
  } finally {
    ws.close();
  }
}

// A slow first welcome (over the driver's 3 s connect stall) must not make an
// invite page open a second socket with the same single-use ticket: the bridge
// would refuse it. The first socket's welcome is held back 4 s here.
async function testTicketSlowWelcome(browser, site) {
  const ws = await startWsServer();
  const context = await browser.newContext({ viewport: { width: 1000, height: 800 } });
  try {
    const page = await context.newPage();
    const errors = [];
    watchErrors(page, errors);
    const sockets = [];
    page.on('websocket', (w) => sockets.push(w.url()));
    let held = false;
    // The route is set before the first navigation: the first welcome is held 4 s.
    await page.routeWebSocket(/\/ws\//, (route) => {
      const server = route.connectToServer();
      route.onMessage((m) => server.send(m));
      server.onMessage((m) => {
        if (!held) { held = true; setTimeout(() => route.send(m), 4000); } else route.send(m);
      });
    });
    await page.goto(site.base + '/app/?url=' + encodeURIComponent(ws.url + '/ws/k9?ticket=t9') + '&invite=x.y', { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => document.body.innerText.includes('connected'), null, { timeout: TIMEOUT });
    await page.waitForTimeout(6000);
    if (sockets.length !== 1) throw new Error(`ticket link opened ${sockets.length} sockets for one page load`);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

function newSiteState(portal) {
  return { portal, rateLimited: false, sessions: PORTAL_ROWS, invites: [], invitePosts: [], deletes: [], loggedOut: 0,
    push: 'on', pushKeyRequests: [], pushPosts: [], pushDeletes: [],
    cloud: 'on', accountGets: 0, accounts: [], accountPosts: [], accountDeletes: [], refusedTokens: ['ghp_refused'],
    repos: REPOS, repoQueries: [], runs: [], runsOff: false, runPosts: [], runActions: [],
    vault: 'on', credentials: [{ id: 'anthropic', set: false }, { id: 'openai', set: false }], vaultPuts: [], vaultDeletes: [] };
}

// The Cloud fixtures: five repos, two to a page (the bridge pages 50).
const REPO_PAGE = 2;
const REPOS = [
  { full_name: 'acme/app', private: false, default_branch: 'main', pushed_at: '2026-11-14T08:00:00Z' },
  { full_name: 'acme/api', private: true, default_branch: 'dev', pushed_at: '2026-11-13T08:00:00Z' },
  { full_name: 'acme/web', private: false, default_branch: 'main', pushed_at: '2026-11-12T08:00:00Z' },
  { full_name: 'acme/docs', private: false, default_branch: 'main', pushed_at: '2026-11-11T08:00:00Z' },
  { full_name: 'acme/tools', private: false, default_branch: 'main', pushed_at: '2026-11-10T08:00:00Z' },
];

// Web push fixtures: the bridge's VAPID public key (65 bytes from 0x04) and the
// browser's subscription keys, as the stub hands them out.
const PUSH_KEY_BYTES = Buffer.concat([Buffer.from([4]), Buffer.alloc(64, 7)]);
const PUSH_KEY = PUSH_KEY_BYTES.toString('base64url');
const P256DH_BYTES = Buffer.concat([Buffer.from([4]), Buffer.from(Array.from({ length: 64 }, (_, i) => (i * 3) & 255))]);
const AUTH_BYTES = Buffer.from(Array.from({ length: 16 }, (_, i) => 160 + i));
const PUSH_ENDPOINT = 'https://fcm.googleapis.com/fcm/send/test-endpoint';
const PUSH_BLOCKED = 'notifications are blocked for this site';

// The browser's push APIs, stubbed before the page's scripts run. `options`:
// permission (the answer to requestPermission), subscribed (a subscription
// already exists), noPush (no PushManager), ios (an iPhone tab, not standalone).
// The stub records what the page asked for in window.__push.
function pushStub(options) {
  return `(() => {
    const opts = ${JSON.stringify(options)};
    const state = { permission: opts.permission || 'default', answer: opts.answer || 'granted',
      subscription: null, subscribeOptions: null, unsubscribed: 0, registered: null };
    window.__push = state;
    const keyBytes = (bytes) => new Uint8Array(bytes).buffer;
    const makeSub = () => ({
      endpoint: ${JSON.stringify(PUSH_ENDPOINT)},
      getKey: (name) => keyBytes(name === 'p256dh' ? ${JSON.stringify([...P256DH_BYTES])} : ${JSON.stringify([...AUTH_BYTES])}),
      unsubscribe: async () => { state.subscription = null; state.unsubscribed += 1; return true; },
    });
    if (opts.subscribed) { state.subscription = makeSub(); state.registered = ['/app/sw.js', '/app/']; }
    const pushManager = {
      getSubscription: async () => state.subscription,
      subscribe: async (o) => {
        state.subscribeOptions = { userVisibleOnly: o.userVisibleOnly, key: Array.from(new Uint8Array(o.applicationServerKey)) };
        state.subscription = makeSub();
        return state.subscription;
      },
    };
    const registration = { scope: '/app/', pushManager };
    const serviceWorker = {
      register: async (url, o) => { state.registered = [url, o.scope]; return registration; },
      getRegistration: async (scope) => (state.registered && scope === '/app/' ? registration : undefined),
    };
    Object.defineProperty(navigator, 'serviceWorker', { get: () => serviceWorker, configurable: true });
    if (!opts.noPush) window.PushManager = function PushManager() {};
    else window.PushManager = undefined;
    window.Notification = {
      get permission() { return state.permission; },
      requestPermission: async () => { state.permission = state.answer; return state.permission; },
    };
    if (opts.ios) {
      Object.defineProperty(navigator, 'userAgent', { get: () => 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15' , configurable: true });
    }
  })();`;
}

// A live page (session attached) with the push stub; `cookie` signs the owner
// in, `query` adds an invite. Returns the page's context, page and errors.
async function openPushPage(browser, site, { cookie = true, query = '', push = {} } = {}) {
  const context = await browser.newContext({ viewport: { width: 1000, height: 800 } });
  if (cookie) await context.addCookies([{ name: 'modron_owner', value: COOKIE, url: site.base }]);
  await context.addInitScript(pushStub(push));
  const page = await context.newPage();
  const errors = [];
  watchErrors(page, errors);
  await page.goto(site.base + '/app/?url=' + encodeURIComponent(site.ws.url) + '&session=' + SESSION + query, { waitUntil: 'domcontentloaded' });
  return { context, page, errors };
}

// Opens Settings on a live page once its stream has shown.
async function openSettings(page) {
  await page.waitForFunction((s) => document.body.innerText.includes(s), STREAM, { timeout: TIMEOUT });
  await tapLabel(page, 'settings');
  await page.waitForFunction(() => document.body.innerText.includes('settings'), null, { timeout: TIMEOUT });
}

// Waits until the page's probe of the key route has answered (the row's state
// is set after that), then a short settle for the row to paint.
async function waitForProbe(site, count = 1) {
  const end = Date.now() + TIMEOUT;
  while (site.state.pushKeyRequests.length < count && Date.now() < end) await new Promise((r) => setTimeout(r, 50));
  await new Promise((r) => setTimeout(r, 400));
}

const expectedBody = (extra = {}) => ({ endpoint: PUSH_ENDPOINT, keys: { p256dh: P256DH_BYTES.toString('base64url'), auth: AUTH_BYTES.toString('base64url') }, ...extra });
// Structural equality (the key order of the JSON is not part of the contract).
function sameBody(actual, expected) {
  try { assert.deepStrictEqual(actual, expected); return true; } catch { return false; }
}

// The row is hidden where the key route answers 503.
async function testPushHiddenOn503(browser, site) {
  site.state.push = 'off';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site);
  try {
    await openSettings(page);
    await waitForProbe(site);
    const text = await page.evaluate(() => document.body.innerText);
    if (text.includes('notify this device')) throw new Error('row shown on 503');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// Off on a fresh device; a tap subscribes and POSTs the exact body; the row
// turns on. The registration is /app/sw.js at scope /app/.
async function testPushTapSubscribes(browser, site) {
  site.state.push = 'on';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site, { push: { permission: 'default', answer: 'granted' } });
  try {
    await openSettings(page);
    await waitRow(page, 'notify this device', 'off');
    await tapPushSwitch(page);
    await waitRow(page, 'notify this device', 'on');
    if (site.state.pushPosts.length !== 1) throw new Error('subscribe posts: ' + site.state.pushPosts.length);
    const post = site.state.pushPosts[0];
    if (!sameBody(post, expectedBody())) throw new Error('subscribe body was ' + JSON.stringify(post));
    const stub = await page.evaluate(() => window.__push);
    if (stub.subscribeOptions.userVisibleOnly !== true) throw new Error('userVisibleOnly not true');
    if (JSON.stringify(stub.subscribeOptions.key) !== JSON.stringify([...PUSH_KEY_BYTES])) throw new Error('applicationServerKey is not the bridge key');
    if (JSON.stringify(stub.registered) !== JSON.stringify(['/app/sw.js', '/app/'])) throw new Error('registered ' + JSON.stringify(stub.registered));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// A blocked permission: the tap sends nothing, the row stays off, the reason shows.
async function testPushDeniedStaysOff(browser, site) {
  site.state.push = 'on';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site, { push: { permission: 'default', answer: 'denied' } });
  try {
    await openSettings(page);
    await waitRow(page, 'notify this device', 'off');
    await tapPushSwitch(page);
    await page.waitForFunction((s) => document.body.innerText.includes(s), PUSH_BLOCKED, { timeout: TIMEOUT });
    await waitForProbe(site, 0);
    if (site.state.pushPosts.length !== 0) throw new Error('a denied permission posted a subscription');
    if (!(await rowHas(page, 'notify this device', 'off'))) throw new Error('row left off was not shown');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// A subscription that already exists shows on at load (from getSubscription);
// a second tap unsubscribes the browser and DELETEs the endpoint.
async function testPushSecondTapUnsubscribes(browser, site) {
  site.state.push = 'on';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site, { push: { permission: 'granted', subscribed: true } });
  try {
    await openSettings(page);
    await waitRow(page, 'notify this device', 'on');
    await tapPushSwitch(page);
    await waitRow(page, 'notify this device', 'off');
    const stub = await page.evaluate(() => window.__push);
    if (stub.unsubscribed !== 1) throw new Error('unsubscribe calls: ' + stub.unsubscribed);
    if (site.state.pushDeletes.length !== 1 || site.state.pushDeletes[0].endpoint !== PUSH_ENDPOINT || Object.keys(site.state.pushDeletes[0]).length !== 1) {
      throw new Error('DELETE body was ' + JSON.stringify(site.state.pushDeletes));
    }
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// A viewer invite: the key route refuses it (403), so the row is hidden. The
// 403 is the page's own probe, so that one console line is expected here.
async function testPushViewerInviteHidden(browser, site) {
  site.state.push = 'on';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site, { cookie: false, query: '&invite=viewer-token' });
  try {
    await openSettings(page);
    await waitForProbe(site);
    const text = await page.evaluate(() => document.body.innerText);
    if (text.includes('notify this device')) throw new Error('row shown to a viewer');
    if (site.state.pushKeyRequests[0] !== 'viewer-token') throw new Error('probe did not carry the invite');
    const unexpected = errors.filter((e) => !e.includes('status of 403'));
    if (unexpected.length) throw new Error('page errors: ' + unexpected.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// An approver invite: the row shows, and a tap posts the invite with the keys.
async function testPushApproverInvite(browser, site) {
  site.state.push = 'on';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site, { cookie: false, query: '&invite=approver-token', push: { permission: 'granted' } });
  try {
    await openSettings(page);
    await waitRow(page, 'notify this device', 'off');
    await tapPushSwitch(page);
    await waitRow(page, 'notify this device', 'on');
    const post = site.state.pushPosts[0];
    if (!sameBody(post, expectedBody({ invite: 'approver-token' }))) throw new Error('approver body was ' + JSON.stringify(post));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

// An iPhone tab without push: the row shows unavailable with the Home Screen hint.
async function testPushIosTabHint(browser, site) {
  site.state.push = 'on';
  const ws = await startWsServer();
  site.ws = ws;
  const { context, page, errors } = await openPushPage(browser, site, { push: { noPush: true, ios: true } });
  try {
    await openSettings(page);
    await waitRow(page, 'notify this device', 'unavailable');
    await page.waitForFunction(() => document.body.innerText.includes('add this page to the Home Screen first'), null, { timeout: TIMEOUT });
    if (site.state.pushPosts.length !== 0) throw new Error('an unavailable row posted');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
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

// Opens the live page on an idle session (no streamed text): waits for the
// connection label instead of the stream.
async function openIdle(browser, base, ws) {
  const query = `?url=${encodeURIComponent(ws.url)}&session=${SESSION}`;
  const opened = await openPage(browser, base + '/' + query);
  await opened.page.waitForFunction(() => document.body.innerText.includes('connected'), null, { timeout: TIMEOUT });
  return opened;
}

// Clicks the page element whose whole text is `label`.
async function tapLabel(page, label) {
  await page.getByText(label, { exact: true }).first().click();
}

// The web draws a settings or credential row's label and value apart (ui.rs
// value_pair), so the page text has no "label: value" run. A row is checked by
// its label leaf and a value leaf inside the same small row.
const rowIncludes = ([label, value]) => {
  const leaves = [...document.querySelectorAll('body *')].filter((e) => e.childElementCount === 0);
  const words = (el) => [...el.querySelectorAll('*')].filter((e) => e.childElementCount === 0).map((e) => e.textContent.trim()).filter(Boolean);
  return leaves.some((leaf) => {
    if (leaf.textContent.trim() !== label) return false;
    // The row is the nearest ancestor whose words are exactly the label and the value
    // (a picker's header and rows share a larger ancestor, which holds more words).
    for (let el = leaf.parentElement, i = 0; el && i < 5; el = el.parentElement, i++) {
      const w = words(el);
      if (w.length === 2 && w.includes(label) && w.includes(value)) return true;
    }
    return false;
  });
};
async function waitRow(page, label, value) {
  await page.waitForFunction(rowIncludes, [label, value], { timeout: TIMEOUT });
}
async function rowHas(page, label, value) {
  return page.evaluate(rowIncludes, [label, value]);
}
// The push row's control is its switch (role switch, named by the row's label).
async function tapPushSwitch(page) {
  await page.getByRole('switch', { name: 'notify this device', exact: true }).click();
}

// The approval type-ahead guard (app/src/chat.rs, APPROVAL_KEY_GUARD, 300 ms):
// a tap or key in the first 300 ms of a prompt is ignored. Waits on the page
// clock until the window has closed, as a user reading the prompt would. The
// observer stamps when "allow once" first shows; a prompt already on screen
// is stamped now, which only makes the wait longer.
async function waitPastPromptGuard(page) {
  await page.evaluate(() => {
    const shown = () => !!document.body && document.body.innerText.includes('allow once');
    if (window.__promptSeen === undefined) {
      window.__promptSeen = null;
      new MutationObserver(() => {
        if (shown()) {
          if (window.__promptSeen === null) window.__promptSeen = Date.now();
        } else {
          window.__promptSeen = null;
        }
      }).observe(document, { subtree: true, childList: true, characterData: true });
    }
    if (shown() && window.__promptSeen === null) window.__promptSeen = Date.now();
  });
  await page.waitForFunction(
    () => window.__promptSeen === null || Date.now() - window.__promptSeen >= 300,
    null,
    { timeout: TIMEOUT }
  );
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

// Esc on an idle composer with text clears the text and sends no frame.
async function testEscClearsDraft(browser, base) {
  const ws = await startWsServer({ live: { running: false }, quiet: true });
  try {
    const { page, errors } = await openIdle(browser, base, ws);
    await page.locator('input:not([type=file]), textarea').first().click();
    await page.keyboard.type('zz');
    await page.waitForFunction(() => document.activeElement.value === 'zz', null, { timeout: TIMEOUT });
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => document.activeElement.value === '', null, { timeout: TIMEOUT });
    if (ws.received.some((f) => String(f).includes('interrupt'))) throw new Error('esc sent: ' + ws.received.join(','));
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
    await waitPastPromptGuard(page);
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

// Approval type-ahead guard (chat.rs, the same window as the keys): a tap on
// a fresh prompt decides nothing; a tap after the 300 ms window approves. An
// init script records the page's clock when the prompt text first shows.
async function testApprovalGuard(browser, base) {
  const ws = await startWsServer({ after: [APPROVAL] });
  try {
    const query = `?url=${encodeURIComponent(ws.url)}&session=${SESSION}`;
    const page = await browser.newPage({ viewport: { width: 1000, height: 800 } });
    const errors = [];
    watchErrors(page, errors);
    await page.addInitScript(() => {
      window.__promptSeen = null;
      new MutationObserver(() => {
        if (window.__promptSeen === null && document.body && document.body.innerText.includes('allow once')) {
          window.__promptSeen = Date.now();
        }
      }).observe(document, { subtree: true, childList: true, characterData: true });
    });
    await page.goto(base + '/' + query, { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => window.__promptSeen !== null, null, { timeout: TIMEOUT });
    const early = await page.evaluate(() => Date.now() - window.__promptSeen < 300);
    if (!early) throw new Error('the tap came after the 300 ms window: the guard case cannot run');
    await tapLabel(page, 'allow once');
    // Wait on the page's clock until the window has closed.
    await page.waitForFunction(() => Date.now() - window.__promptSeen >= 300, null, { timeout: TIMEOUT });
    // Absence check: an accepted tap's frame would have crossed the local
    // socket by now; this short wait is the only time-based step in the case.
    await new Promise((r) => setTimeout(r, 250));
    if (ws.frames.some((f) => f.type === 'approve')) throw new Error('a tap inside the window approved');
    await tapLabel(page, 'allow once');
    const frame = await waitFrame(ws, 'approve');
    if (frame.decision !== 'allow_once') throw new Error('decision was ' + frame.decision);
    if (ws.frames.filter((f) => f.type === 'approve').length !== 1) throw new Error('more than one approve frame');
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
  // UX-2: a task is a neutral card, so the primary shows where the design
  // puts it (the focus ring, the primary buttons, the streaming rule). The
  // probe is any element whose text, background or border takes the primary.
  const found = await page.evaluate(() => {
    const gold = 'rgb(217, 165, 32)';
    return [...document.querySelectorAll('*')].some((e) => {
      const s = getComputedStyle(e);
      return s.color === gold || s.backgroundColor === gold || s.borderTopColor === gold || s.borderLeftColor === gold;
    });
  });
  if (!found) throw new Error('no element takes the oculus primary (rgb(217, 165, 32))');
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

// The chat on a phone (UX-2): the send control is a real button at least 44 px
// tall, the composer sits at the bottom of the window, the page does not
// scroll sideways; a pending approval shows its three words as buttons.
async function testChatWeb(browser, base) {
  {
    const ws = await startWsServer({ quiet: true });
    try {
      const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
      const errors = [];
      watchErrors(page, errors);
      await page.goto(base + '/?url=' + encodeURIComponent(ws.url) + '&session=' + SESSION, { waitUntil: 'domcontentloaded' });
      await page.waitForFunction(() => document.body.innerText.includes('connected'), null, { timeout: TIMEOUT });
      await page.locator('input:not([type=file]), textarea').first().click();
      await page.keyboard.type('hi');
      await page.waitForFunction(() => document.activeElement.value === 'hi', null, { timeout: TIMEOUT });
      const r = await page.evaluate(() => {
        const send = [...document.querySelectorAll('[role=button]')].find((e) => (e.innerText || '').trim() === 'send');
        const field = document.querySelector('input:not([type=file]), textarea');
        const box = send ? send.getBoundingClientRect() : null;
        return {
          sendHeight: box ? box.height : 0,
          fieldBottom: field.getBoundingClientRect().bottom,
          innerHeight: window.innerHeight,
          overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth,
        };
      });
      if (r.sendHeight < 44) throw new Error(`send button is ${r.sendHeight} px tall (want 44)`);
      if (r.fieldBottom > r.innerHeight || r.innerHeight - r.fieldBottom > 160) {
        throw new Error(`composer bottom ${Math.round(r.fieldBottom)} is not pinned in ${r.innerHeight}`);
      }
      if (r.overflowX) throw new Error('the chat scrolls sideways on a phone');
      if (errors.length) throw new Error('page errors: ' + errors.join('; '));
      await page.close();
    } finally {
      ws.close();
    }
  }
  {
    const ws = await startWsServer({ after: [APPROVAL] });
    try {
      const { page, errors } = await openLive(browser, base, ws);
      await page.waitForFunction(() => document.body.innerText.includes('allow once'), null, { timeout: TIMEOUT });
      const buttons = await page.evaluate(() => [...document.querySelectorAll('[role=button]')].map((e) => (e.innerText || '').trim()));
      for (const want of ['allow once', 'allow all', 'deny']) {
        if (!buttons.includes(want)) throw new Error(`approval word "${want}" is not a button (have ${buttons.join(' | ')})`);
      }
      if (errors.length) throw new Error('page errors: ' + errors.join('; '));
      await page.close();
    } finally {
      ws.close();
    }
  }
}

// Esc on a pending approval denies it (the third decision), it does not interrupt.
async function testEscDeniesApproval(browser, base) {
  const ws = await startWsServer({ after: [APPROVAL] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('allow once'), null, { timeout: TIMEOUT });
    await waitPastPromptGuard(page);
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

// Mouse wheel over the transcript scrolls it to the older rows. The page
// does not scroll natively, and the transcript is a virtual list, so only
// the rows in view are in the DOM: the oldest row must not be visible first.
async function testWheelScrollsTranscript(browser, base) {
  const after = [];
  for (let i = 0; i < 60; i++) {
    after.push({ session: SESSION, seq: 100 + i, type: 'event', event: { type: 'task', text: `wheel row ${i}` } });
  }
  const ws = await startWsServer({ after });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('wheel row 59'), null, { timeout: TIMEOUT });
    if ((await page.evaluate(() => document.body.innerText)).includes('wheel row 0')) {
      throw new Error('precondition: the oldest row is already in view, the history is too short');
    }
    for (let i = 0; i < 80; i++) {
      if ((await page.evaluate(() => document.body.innerText)).includes('wheel row 0')) break;
      await page.mouse.move(500, 300);
      await page.mouse.wheel(0, -240);
      await page.waitForTimeout(100);
    }
    const text = await page.evaluate(() => document.body.innerText);
    if (!text.includes('wheel row 0')) throw new Error('the wheel did not reach the oldest row');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Every pressable control and the composer are inside the window at a
// desktop and a phone size, with an approval pending, a run in progress and
// two queued messages (the layout the user has to reach).
async function testLayoutInViewport(browser, base) {
  const ws = await startWsServer({
    live: { running: true },
    after: [APPROVAL, { type: 'queue_changed', queued: ['one', 'two'] }],
  });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('[2] two'), null, { timeout: TIMEOUT });
    for (const [width, height] of [[1280, 720], [390, 844]]) {
      await page.setViewportSize({ width, height });
      await page.waitForTimeout(400);
      const report = await page.evaluate(() => {
        const labels = ['allow once', 'allow all', 'deny', 'stop', 'interrupt', 'queue'];
        const els = [...document.querySelectorAll('[role=button], input, textarea, span, div, p')].filter((el) => {
          if (el.querySelector('[role=button], input, textarea')) return false;
          const text = (el.innerText || el.placeholder || '').trim();
          return el.matches('[role=button], input, textarea') || labels.includes(text) || /^\[\d+\] /.test(text);
        });
        const boxes = els.map((el) => {
          const r = el.getBoundingClientRect();
          return {
            label: (el.innerText || el.placeholder || el.tagName).trim().slice(0, 40),
            visible: r.width > 0 && r.height > 0,
            top: r.top, left: r.left, bottom: r.bottom, right: r.right,
          };
        }).filter((b) => b.visible);
        return { vw: window.innerWidth, vh: window.innerHeight, boxes };
      });
      const names = report.boxes.map((b) => b.label);
      for (const want of ['allow once', '[1] one', '[2] two', 'Message']) {
        if (!names.some((n) => n === want || (want.startsWith('[') && n.startsWith(want)))) {
          throw new Error(`${width}x${height}: no control "${want}" (have ${names.join(' | ')})`);
        }
      }
      const outside = report.boxes.filter((b) => b.top < 0 || b.left < 0 || b.bottom > report.vh || b.right > report.vw);
      if (outside.length) {
        throw new Error(`${width}x${height} (viewport ${report.vw}x${report.vh}): outside: ` +
          outside.map((b) => `${b.label} [${Math.round(b.top)}..${Math.round(b.bottom)}]`).join(', '));
      }
    }
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
  // The openers live in the chat's bar; a screen has "back", not openers.
  await tapLabel(page, 'back');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  await tapLabel(page, 'settings');
  await waitRow(page, 'approval mode', 'ask');
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}
// The theme picker (Settings, theme row): a tap on "oculus light" paints the
// light tokens (oculus.json: background #FFFFFF), a tap on "oculus dark" the
// dark ones (#000000), and the dark choice is read back from localStorage
// after a reload. Rows are matched by their text, which carries a dot when
// current, so the match is a substring.
async function tapText(page, label) {
  await page.getByText(label).first().click();
}
// What the page paints: every distinct background, and the text colour of the
// Settings row "phase" (a plain row, not the selected one).
async function paintSummary(page) {
  return page.evaluate(() => {
    const all = [...document.querySelectorAll('*')];
    const leaf = all.find((e) => e.childElementCount === 0 && e.textContent.trim() === 'phase');
    return {
      backgrounds: [...new Set(all.map((e) => getComputedStyle(e).backgroundColor))],
      text: leaf ? getComputedStyle(leaf).color : null,
    };
  });
}
async function testThemePicker(browser, base) {
  const { page, errors } = await openPage(browser, base + '/');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  await tapText(page, 'settings');
  await waitRow(page, 'approval mode', 'ask');
  await tapLabel(page, 'theme');
  await page.waitForFunction(() => document.body.innerText.includes('oculus light'), null, { timeout: TIMEOUT });
  await tapText(page, 'oculus light');
  await waitRow(page, 'theme', 'oculus light');
  let painted = await paintSummary(page);
  if (!painted.backgrounds.includes('rgb(255, 255, 255)')) throw new Error('light theme paints ' + JSON.stringify(painted));
  await tapLabel(page, 'theme');
  await page.waitForFunction(() => document.body.innerText.includes('oculus dark'), null, { timeout: TIMEOUT });
  await tapText(page, 'oculus dark');
  await waitRow(page, 'theme', 'oculus dark');
  painted = await paintSummary(page);
  if (!painted.backgrounds.includes('rgb(0, 0, 0)')) throw new Error('dark theme paints ' + JSON.stringify(painted));
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  const saved = await page.evaluate(() => localStorage.getItem('modron.theme'));
  if (saved !== 'oculus dark') throw new Error('saved theme is ' + saved);
  await page.waitForFunction(
    () => [...document.querySelectorAll('*')].some((e) => getComputedStyle(e).backgroundColor === 'rgb(0, 0, 0)'),
    null, { timeout: TIMEOUT });
  if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  await page.close();
}

// A session row is pressable: a tap sends an attach frame for that session.
async function testSessionRowAttaches(browser, base) {
  const other = '9c2d7e10-1a2b-4c3d-8e4f-5a6b7c8d9e0f';
  const ws = await startWsServer({ sessions: [{ id: other, title: 'other work', created_at: '', updated_at: '', message_count: 2 }] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    // One socket per page load: the session list and the attach reuse it (each
    // socket opens with one hello frame).
    const hellos = () => ws.received.filter((t) => t === 'hello').length;
    const before = hellos();
    await tapLabel(page, 'sessions');
    await page.waitForFunction(() => document.body.innerText.includes('other work'), null, { timeout: TIMEOUT });
    await page.getByText('other work', { exact: false }).first().click();
    const frame = await waitFrame(ws, 'attach', (f) => f.session_id === other);
    if (frame.session_id !== other) throw new Error('attach carried ' + frame.session_id);
    if (hellos() !== before) throw new Error(`opening a session opened ${hellos() - before} more socket(s)`);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}
// The effort row is pressable: a tap on the effort row (unset) sends set_config
// with effort low (unset -> low).
async function testEffortTapSendsConfig(browser, base) {
  const ws = await startWsServer();
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await tapLabel(page, 'settings');
    await waitRow(page, 'effort', 'unset');
    // The row opens the effort picker (the old terminal's menus); a tap on a level applies it.
    await tapLabel(page, 'effort');
    await page.waitForFunction(() => document.body.innerText.includes('xhigh'), null, { timeout: TIMEOUT });
    await tapText(page, 'low');
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

// PARITY-D2 item 1. "!!ls" sends a shell frame with private true, never a submit.
async function testBangBangPrivate(browser, base) {
  const ws = await startWsServer();
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.keyboard.type('!!ls');
    await page.keyboard.press('Enter');
    const frame = await waitFrame(ws, 'shell', (f) => f.command === 'ls');
    if (frame.private !== true) throw new Error('!!ls frame was not private: ' + JSON.stringify(frame));
    if (ws.received.includes('submit')) throw new Error('a shell line was submitted: ' + ws.received.join(','));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Packet C3f. Typing "/co" in the composer shows the compact suggestion.
// V4: a forked session's row names its parent and turn (the core's words),
// and `/fork 2` sends a fork frame for turn 2 (the tip is `/fork`).
async function testForkRowAndTurn(browser, base) {
  const fork = '9c2d7e10-1a2b-4c3d-8e4f-5a6b7c8d9e0f';
  const ws = await startWsServer({ sessions: [{ id: fork, title: 'retry', created_at: '', updated_at: '', message_count: 4, forked_from: { session_id: SESSION, turn: 2 } }] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await tapLabel(page, 'sessions');
    await page.waitForFunction(() => document.body.innerText.includes('forked from 5b1e8f4a at turn 2'), null, { timeout: TIMEOUT });
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    await page.keyboard.type('/fork 2');
    await page.waitForFunction(() => document.activeElement && document.activeElement.value === '/fork 2', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    const frame = await waitFrame(ws, 'fork', (f) => f.turn === 2);
    if (frame.session_id !== SESSION) throw new Error('fork carried ' + frame.session_id);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Program V Part B: the welcome's project commands are listed after the
// built-ins in the slash popup; `/deploy` is sent, and an unlisted `/word`
// is the local unknown-command notice with no submit frame.
async function testProjectCommands(browser, base) {
  const ws = await startWsServer({ commands: ['deploy'] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/dep');
    await page.waitForFunction(() => document.body.innerText.includes('/deploy [args]'), null, { timeout: TIMEOUT });
    await page.keyboard.press('Control+a');
    await page.keyboard.type('/deploy staging');
    await page.waitForFunction(() => document.activeElement.value === '/deploy staging', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await waitFrame(ws, 'submit', (f) => f.task === '/deploy staging');
    await page.keyboard.type('/depoly');
    await page.waitForFunction(() => document.activeElement.value === '/depoly', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('unknown command'), null, { timeout: TIMEOUT });
    if (ws.frames.some((f) => f.type === 'submit' && f.task === '/depoly')) throw new Error('an unlisted /word was sent');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

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

// A forced disconnect and reconnect (the path that panicked on wasm32 when the
// driver read std time): no page error or console error, before or after it.
async function testNoErrorsAcrossReconnect(browser, base) {
  const ws = await startWsServer({ dropOnce: true });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForTimeout(4000);
    for (let i = 0; i < 250 && ws.received.filter((t) => t === 'attach').length < 2; i++) {
      await new Promise((r) => setTimeout(r, 100));
    }
    if (ws.received.filter((t) => t === 'attach').length < 2) throw new Error('no re-attach after the forced drop');
    await page.waitForTimeout(3000);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// A dropped link (web): the banner reads `reconnecting` with a `retry now`
// button (44 px). While the server refuses, the driver keeps retrying; the
// button reconnects at once and the banner goes.
async function testReconnectingBannerAndRetryNow(browser, base) {
  const ws = await startWsServer({ dropOnce: true, refuseAfterDrop: true });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('reconnecting'), null, { timeout: TIMEOUT });
    const retry = page.getByRole('button', { name: 'retry now', exact: true });
    await retry.waitFor({ timeout: TIMEOUT });
    const box = await retry.boundingBox();
    if (!box || box.height < 44) throw new Error('retry now is under 44 px: ' + JSON.stringify(box));
    const attaches = () => ws.received.filter((t) => t === 'attach').length;
    const before = attaches();
    ws.lift();
    await retry.click();
    for (let i = 0; i < 100 && attaches() <= before; i++) await new Promise((r) => setTimeout(r, 100));
    if (attaches() <= before) throw new Error('retry now did not reconnect');
    await page.waitForFunction(() => !document.body.innerText.includes('reconnecting'), null, { timeout: TIMEOUT });
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// A blank session (web): the welcome is one empty state (the app's title, the
// tagline, the model line). The terminal's key hints are not drawn on the web.
async function testWebWelcomeEmptyState(browser, base) {
  const ws = await startWsServer({ quiet: true });
  try {
    const { page, errors } = await openIdle(browser, base, ws);
    await page.waitForFunction(() => document.body.innerText.includes('a token-efficient Rust coding agent'), null, { timeout: TIMEOUT }).catch(async (e) => {
      throw new Error('no welcome: ' + JSON.stringify(await page.evaluate(() => document.body.innerText)).slice(0, 700) + ' / ' + e.message);
    });
    const text = await page.evaluate(() => document.body.innerText);
    if (!/modron v\S+/.test(text)) throw new Error('the welcome shows no title: ' + text.slice(0, 300));
    if (!/\bmodel\b/.test(text)) throw new Error('the welcome shows no model line');
    if (/shift\+enter|pgup|esc interrupt/i.test(text)) throw new Error('terminal key hints on the web welcome');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Credentials: the screen opens on the first run (nothing signed in). Enter on
// the OAuth row sends auth_start; on the key row the masked entry sends
// auth_set_key with the typed key, and the key leaves the page afterwards.
async function testCredentialsScreen(browser, base) {
  const providers = [
    { id: 'anthropic', label: 'Anthropic', status: 'not configured', oauth_capable: true },
    { id: 'openai', label: 'OpenAI', status: 'not configured', oauth_capable: false },
  ];
  const ws = await startWsServer({ providers });
  try {
    // The first run opens this screen only on a blank, idle session. The
    // scripted session is streaming, so open the screen with /auth.
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/auth');
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await waitRow(page, 'OpenAI', 'not configured');

    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    const started = await waitFrame(ws, 'auth_start', (f) => f.provider === 'anthropic');
    if (started.provider !== 'anthropic') throw new Error('auth_start was ' + JSON.stringify(started));
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(100);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('OpenAI: paste an API key'), null, { timeout: TIMEOUT });
    // Type only once the masked field has the focus, and send only once it
    // holds the whole key: keys typed before the field is focused are lost.
    await page.waitForFunction(() => document.activeElement && document.activeElement.type === 'password', null, { timeout: TIMEOUT });
    await page.keyboard.type('sk-test-123');
    await page.waitForFunction(() => document.activeElement.value === 'sk-test-123', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    const saved = await waitFrame(ws, 'auth_set_key', (f) => f.provider === 'openai');
    if (saved.key !== 'sk-test-123') throw new Error('auth_set_key sent the wrong key');
    await page.waitForFunction(() => !document.body.innerText.includes('OpenAI: paste an API key'), null, { timeout: TIMEOUT });
    const html = await page.content();
    if (html.includes('sk-test-123')) throw new Error('the key stayed on the page');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// The ChatGPT row (the server's `openai-compat`, oauth_capable) starts the
// device flow like Claude's row: Enter sends auth_start, the page shows the
// URL and the one-time code, and the outcome note lands. No key entry.
async function testChatGptSignIn(browser, base) {
  const providers = [
    { id: 'anthropic', label: 'Anthropic', status: 'not configured', oauth_capable: true },
    { id: 'openai-compat', label: 'OpenAI / ChatGPT', status: 'not configured', oauth_capable: true },
  ];
  const ws = await startWsServer({ providers, oauth: true });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/auth');
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('ChatGPT'), null, { timeout: TIMEOUT }).catch(async (e) => {
      throw new Error('the credentials screen shows no ChatGPT row: ' + (await page.evaluate(() => document.body.innerText)).slice(0, 400) + ' / ' + e.message);
    });
    await page.waitForTimeout(300);
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(100);
    await page.keyboard.press('Enter');
    const started = await waitFrame(ws, 'auth_start', (f) => f.provider === 'openai-compat');
    if (started.provider !== 'openai-compat') throw new Error('auth_start was ' + JSON.stringify(started));
    await page.waitForFunction(() => document.body.innerText.includes('WXYZ-1234'), null, { timeout: TIMEOUT }).catch(async (e) => {
      throw new Error('no user code on the page: ' + (await page.evaluate(() => document.body.innerText)).slice(0, 600) + ' / ' + e.message);
    });
    if (!(await page.content()).includes('auth.openai.com/codex/device')) throw new Error('the sign-in URL is not shown');
    await page.waitForFunction(() => document.body.innerText.includes('chatgpt signed in'), null, { timeout: TIMEOUT });
    if (ws.received.includes('auth_set_key')) throw new Error('ChatGPT sign-in sent a key');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// An OAuth-capable row has two buttons (web): `api key` opens the masked entry
// and the key is sent as auth_set_key for that provider; `sign in` starts the
// device flow. Both work on the ChatGPT row, which Enter alone could not reach.
async function testChatGptRowHasKeyAndSignInButtons(browser, base) {
  const providers = [
    { id: 'anthropic', label: 'Anthropic', status: 'not configured', oauth_capable: true },
    { id: 'openai-compat', label: 'OpenAI / ChatGPT', status: 'not configured', oauth_capable: true },
  ];
  const ws = await startWsServer({ providers, oauth: true });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/auth');
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('ChatGPT'), null, { timeout: TIMEOUT });
    const keyButton = page.getByRole('button', { name: 'api key', exact: true });
    await keyButton.nth(1).click();
    await page.waitForFunction(() => document.body.innerText.includes('OpenAI / ChatGPT: paste an API key'), null, { timeout: TIMEOUT });
    await page.waitForFunction(() => document.activeElement && document.activeElement.type === 'password', null, { timeout: TIMEOUT });
    await page.keyboard.type('sk-chatgpt-456');
    await page.waitForFunction(() => document.activeElement.value === 'sk-chatgpt-456', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    const saved = await waitFrame(ws, 'auth_set_key', (f) => f.provider === 'openai-compat');
    if (saved.key !== 'sk-chatgpt-456') throw new Error('auth_set_key sent the wrong key');
    await page.waitForFunction(() => !document.body.innerText.includes('OpenAI / ChatGPT: paste an API key'), null, { timeout: TIMEOUT });
    if (ws.received.includes('auth_start')) throw new Error('the key button started a sign-in');
    await page.getByRole('button', { name: 'sign in', exact: true }).nth(1).click();
    const started = await waitFrame(ws, 'auth_start', (f) => f.provider === 'openai-compat');
    if (started.provider !== 'openai-compat') throw new Error('auth_start was ' + JSON.stringify(started));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// /agent opens the persona picker: the server's personas are its rows, and a
// tap on a row sends set_config with that persona.
async function testPersonaPicker(browser, base) {
  const personas = [
    { name: 'planner', summary: 'plans the work' },
    { name: 'reviewer', summary: 'reviews diffs' },
  ];
  const ws = await startWsServer({ personas });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/agent');
    await page.waitForFunction(() => document.activeElement.value === '/agent', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    await waitFrame(ws, 'personas_req');
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'reviewer - reviews diffs', { timeout: TIMEOUT });
    await tapLabel(page, 'reviewer - reviews diffs');
    const cfg = await waitFrame(ws, 'set_config', (f) => f.patch && f.patch.persona === 'reviewer');
    if (cfg.patch.persona !== 'reviewer') throw new Error('set_config carried the wrong persona');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// Credentials removal: the signed-in row has one remove control (the
// not-configured row has none); remove asks y / n, and y sends auth_remove.
async function testCredentialRemove(browser, base) {
  const providers = [
    { id: 'anthropic', label: 'Anthropic', status: 'configured', oauth_capable: true },
    { id: 'openai', label: 'OpenAI', status: 'not configured', oauth_capable: false },
  ];
  const ws = await startWsServer({ providers });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/auth');
    await page.waitForFunction(() => document.activeElement.value === '/auth', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    await waitRow(page, 'OpenAI', 'not configured');
    const removes = await page.getByText('remove', { exact: true }).count();
    if (removes !== 1) throw new Error(`expected one remove control, saw ${removes}`);
    await tapLabel(page, 'remove');
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'remove Anthropic credentials?', { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    const removed = await waitFrame(ws, 'auth_remove');
    if (removed.provider !== 'anthropic') throw new Error('auth_remove was ' + JSON.stringify(removed));
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// A confirmation is a modal dialog (web): the question without the terminal's
// `y / n` hint, the y and n buttons. Tab keeps the focus inside it, Escape
// closes it without sending anything, and y sends the removal.
async function testConfirmDialog(browser, base) {
  const providers = [
    { id: 'anthropic', label: 'Anthropic', status: 'configured', oauth_capable: true },
  ];
  const ws = await startWsServer({ providers });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/auth');
    await page.waitForFunction(() => document.activeElement.value === '/auth', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    await waitRow(page, 'Anthropic', 'configured');
    await tapLabel(page, 'remove');
    const dialog = page.getByRole('dialog');
    await dialog.waitFor({ timeout: TIMEOUT });
    const shown = await dialog.innerText();
    if (!shown.includes('remove Anthropic credentials?')) throw new Error('the dialog asks: ' + shown);
    if (shown.includes('y / n')) throw new Error('the dialog shows the terminal key hint: ' + shown);
    // The dialog slides in (the platform's modal motion) and the page behind
    // it becomes inert: that is the focus trap (keyboard Tab does not walk the
    // page on this target at all, with or without a dialog).
    await page.waitForFunction(() => { const d = document.querySelector('[role="dialog"]'); return d && !d.style.transform; }, null, { timeout: TIMEOUT });
    const trapped = await page.evaluate(() => {
      const d = document.querySelector('[role="dialog"]');
      const behind = [...document.querySelectorAll('[inert]')].filter((e) => !d.contains(e));
      return { inert: behind.length, dialogHasStops: d.querySelectorAll('[role="button"]').length };
    });
    if (trapped.inert === 0) throw new Error('nothing behind the dialog is inert: no focus trap');
    if (trapped.dialogHasStops < 2) throw new Error('the dialog has no buttons to focus');
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.querySelector('[role="dialog"]'), null, { timeout: TIMEOUT });
    if (ws.received.includes('auth_remove')) throw new Error('Escape removed the credential');
    await tapLabel(page, 'remove');
    await dialog.waitFor({ timeout: TIMEOUT });
    await dialog.getByRole('button', { name: 'y', exact: true }).click();
    const removed = await waitFrame(ws, 'auth_remove');
    if (removed.provider !== 'anthropic') throw new Error('auth_remove was ' + JSON.stringify(removed));
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
    await page.waitForFunction(() => document.body.innerText.includes('delete other work?'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    const frame = await waitFrame(ws, 'delete_session', (f) => f.session_id === other);
    if (frame.session_id !== other) throw new Error('delete carried ' + frame.session_id);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// /agent's first row clears the persona: a tap sends set_config with an empty name.
async function testPersonaClear(browser, base) {
  const ws = await startWsServer({ personas: [{ name: 'reviewer', summary: 'reviews diffs' }] });
  try {
    const { page, errors } = await openLive(browser, base, ws);
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/agent');
    await page.waitForFunction(() => document.activeElement.value === '/agent', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    await waitFrame(ws, 'personas_req');
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'no persona', { timeout: TIMEOUT });
    // The first row is selected, so its text carries the "> " marker: match it inexactly.
    await page.getByText('no persona').first().click();
    const cfg = await waitFrame(ws, 'set_config', (f) => f.patch && f.patch.persona === '');
    if (cfg.patch.persona !== '') throw new Error('set_config did not carry an empty persona');
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
    await page.waitForFunction(() => document.body.innerText.includes('kill reviewer?'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    const kill = await waitFrame(ws, 'kill_agent', (f) => f.agent === 'agent-review');
    if (kill.agent !== 'agent-review') throw new Error('kill carried ' + kill.agent);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// The attach control opens the file chooser for an image. The picked file is
// one chip; the next submit carries it as one image attachment, and the chip
// goes once the send is accepted (APP-4 item 3).
async function testAttachSendsOneImage(browser, base) {
  // An idle session: attachments ride only on a submit (a steer refuses them).
  const ws = await startWsServer({ live: { running: false }, quiet: true });
  try {
    const { page, errors } = await openIdle(browser, base, ws);
    await tapLabel(page, 'attach');
    await page.waitForSelector('input[type=file]', { state: 'attached', timeout: TIMEOUT });
    const png = Buffer.from('89504e470d0a1a0a0000000d4948445200000001000000010806000000', 'hex');
    await page.setInputFiles('input[type=file]', { name: 'shot.png', mimeType: 'image/png', buffer: png });
    await page.waitForFunction(() => document.body.innerText.includes('shot.png'), null, { timeout: TIMEOUT });
    // The picker left focus on the hidden file input: give the composer the focus back.
    await page.locator('input:not([type=file]), textarea').first().click();
    await page.keyboard.type('hi');
    await page.waitForFunction(() => document.activeElement.value === 'hi', null, { timeout: TIMEOUT });
    await page.waitForTimeout(300);
    await page.keyboard.press('Enter');
    const frame = await waitFrame(ws, 'submit').catch(async (e) => {
      const shown = await page.evaluate(() => { const a = document.activeElement; return [a && a.tagName, a && a.type, a && a.value, document.querySelectorAll('input').length].join(' | '); });
      throw new Error(e.message + '\nfocus: ' + shown + '\nframes: ' + ws.received.join(','));
    });
    if (!Array.isArray(frame.attachments) || frame.attachments.length !== 1) {
      throw new Error('submit carried ' + JSON.stringify(frame.attachments));
    }
    const image = frame.attachments[0];
    if (image.kind !== 'image' || image.media_type !== 'image/png') {
      throw new Error('attachment was ' + JSON.stringify({ kind: image.kind, media_type: image.media_type }));
    }
    await page.waitForFunction(() => !document.body.innerText.includes('shot.png'), null, { timeout: 8000 }).catch(async () => { throw new Error('the chip or its notice stayed; page text: ' + JSON.stringify((await page.evaluate(() => document.body.innerText)).slice(0, 600))); });
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    await page.close();
  } finally {
    ws.close();
  }
}

// A leaf element whose whole text is `text` (the web controls are text).
async function waitLeaf(page, text) {
  await page.waitForFunction((t) => [...document.querySelectorAll('*')].some((el) => el.childElementCount === 0 && el.textContent.trim() === t), text, { timeout: TIMEOUT });
}

// Waits until `pred()` holds (the scripted site records each request).
async function waitUntil(pred, what) {
  const end = Date.now() + TIMEOUT;
  while (!pred()) {
    if (Date.now() > end) throw new Error('timed out waiting for ' + what);
    await new Promise((r) => setTimeout(r, 50));
  }
}

// The owner's portal list, with the Cloud screen open (the probe answered 200).
async function openCloud(page) {
  await waitLeaf(page, 'cloud');
  await tapLabel(page, 'cloud');
  await page.waitForFunction(() => document.body.innerText.includes('github accounts'), null, { timeout: TIMEOUT });
}

// Waits until the masked field has the focus, then types `text` into it.
async function typeMasked(page, text) {
  await page.waitForFunction(() => document.activeElement && document.activeElement.type === 'password', null, { timeout: TIMEOUT });
  await page.keyboard.type(text);
  await page.waitForFunction((t) => document.activeElement.value === t, text, { timeout: TIMEOUT });
}

// The Cloud entry is not drawn where the accounts route answers 503.
async function testCloudHiddenOn503(browser, site) {
  site.state.cloud = 'off';
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await page.waitForFunction((s) => document.body.innerText.includes(s), 'fix router', { timeout: TIMEOUT });
    await waitUntil(() => site.state.accountGets >= 1, 'the accounts probe');
    await page.waitForTimeout(200);
    if (await page.getByText('cloud', { exact: true }).count()) throw new Error('the cloud entry showed on a 503');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// A masked token is POSTed and never shown; a refused token shows GitHub's
// status from the 400, and the next token links the account.
async function testCloudAddAccount(browser, site) {
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await openCloud(page);
    await tapLabel(page, 'add account');
    await typeMasked(page, 'ghp_refused');
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('GitHub refused the token (status 401)'), null, { timeout: TIMEOUT });
    await waitUntil(() => site.state.accountPosts.length === 1, 'the refused POST');
    if (!sameBody(site.state.accountPosts[0], { token: 'ghp_refused' })) throw new Error('POST body was ' + JSON.stringify(site.state.accountPosts[0]));
    if ((await page.content()).includes('ghp_refused')) throw new Error('the refused token stayed on the page');
    await tapLabel(page, 'add account');
    await typeMasked(page, 'ghp_good');
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('octocat \u00b7 token'), null, { timeout: TIMEOUT });
    await waitUntil(() => site.state.accountPosts.length === 2, 'the second POST');
    if (!sameBody(site.state.accountPosts[1], { token: 'ghp_good' })) throw new Error('POST body was ' + JSON.stringify(site.state.accountPosts[1]));
    if ((await page.content()).includes('ghp_good')) throw new Error('the token stayed on the page');
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// Remove asks y / n first, then DELETEs that account.
async function testCloudRemoveAccount(browser, site) {
  site.state.accounts = [{ id: 'a1', login: 'octocat', kind: 'token' }];
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await openCloud(page);
    await page.waitForFunction(() => document.body.innerText.includes('octocat \u00b7 token'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'remove');
    await page.waitForFunction(() => document.body.innerText.includes('remove octocat?'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    await waitUntil(() => site.state.accountDeletes.includes('a1'), 'the DELETE');
    await page.waitForFunction(() => document.body.innerText.includes('no github accounts yet'), null, { timeout: TIMEOUT });
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// The picker sends its text as `q` once it pauses (one request, not one per
// key), and asks for the next page when its last row is selected.
async function testCloudRepoPicker(browser, site) {
  site.state.accounts = [{ id: 'a1', login: 'octocat', kind: 'token' }];
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await openCloud(page);
    await tapLabel(page, 'choose a repo');
    await page.waitForFunction(() => document.body.innerText.includes('acme/app \u00b7 public'), null, { timeout: TIMEOUT });
    await waitUntil(() => site.state.repoQueries.some((r) => r.q === '' && r.page === 1 && r.account === 'a1'), 'the first page');
    await page.keyboard.type('ac');
    await waitUntil(() => site.state.repoQueries.some((r) => r.q === 'ac' && r.page === 1), 'the filtered page');
    if (site.state.repoQueries.some((r) => r.q === 'a')) throw new Error('a query went out per key: ' + JSON.stringify(site.state.repoQueries));
    // Two rows on the first page: the second is the last, so selecting it pages on.
    await page.waitForFunction(() => document.body.innerText.includes('acme/api \u00b7 private'), null, { timeout: TIMEOUT });
    await page.keyboard.press('ArrowDown');
    await waitUntil(() => site.state.repoQueries.some((r) => r.q === 'ac' && r.page === 2), 'the next page');
    await page.waitForFunction(() => document.body.innerText.includes('acme/docs \u00b7 public'), null, { timeout: TIMEOUT });
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// A pick fills the run's repo and default branch; the branch can be edited;
// start posts {account, repo, branch} and the run's row appears.
async function testCloudStartRun(browser, site) {
  site.state.accounts = [{ id: 'a1', login: 'octocat', kind: 'token' }];
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await openCloud(page);
    await tapLabel(page, 'choose a repo');
    await page.waitForFunction(() => document.body.innerText.includes('acme/app \u00b7 public'), null, { timeout: TIMEOUT });
    await page.getByText('acme/app', { exact: false }).first().click();
    await page.waitForFunction(() => document.body.innerText.includes('repo: acme/app'), null, { timeout: TIMEOUT });
    await page.waitForFunction(() => document.querySelector('input') && document.querySelector('input').value === 'main', null, { timeout: TIMEOUT });
    await page.locator('input').first().fill('dev');
    await page.waitForFunction(() => document.querySelector('input').value === 'dev', null, { timeout: TIMEOUT });
    await tapLabel(page, 'start run');
    await waitUntil(() => site.state.runPosts.length === 1, 'the start POST');
    if (!sameBody(site.state.runPosts[0], { account: 'a1', repo: 'acme/app', branch: 'dev' })) {
      throw new Error('start body was ' + JSON.stringify(site.state.runPosts[0]));
    }
    await page.waitForFunction(() => document.body.innerText.includes('acme/app \u00b7 dev \u00b7 starting'), null, { timeout: TIMEOUT });
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// A running run offers stop and open; a failed one offers wake. Each control
// hits its route; open goes to the run's session address.
async function testCloudRunActions(browser, site) {
  site.state.accounts = [{ id: 'a1', login: 'octocat', kind: 'token' }];
  site.state.runs = [
    { id: 'r1', repo: 'acme/app', account: 'a1', branch: 'main', state: 'running', reason: '', created_at: '2026-11-15T10:20:30Z', session_url: '/app/?opened=run1' },
    { id: 'r2', repo: 'acme/api', account: 'a1', branch: 'dev', state: 'failed', reason: 'exit 2', created_at: '2026-11-14T08:00:00Z', session_url: '' },
  ];
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await openCloud(page);
    await page.waitForFunction(() => document.body.innerText.includes('acme/api \u00b7 dev \u00b7 failed \u00b7 exit 2'), null, { timeout: TIMEOUT });
    if (await page.getByText('open', { exact: true }).count() !== 1) throw new Error('only the run with a session offers open');
    await tapLabel(page, 'wake');
    await waitUntil(() => site.state.runActions.some((a) => a.id === 'r2' && a.action === 'wake'), 'the wake route');
    await page.getByText('stop', { exact: true }).first().click();
    await waitUntil(() => site.state.runActions.some((a) => a.id === 'r1' && a.action === 'stop'), 'the stop route');
    await page.getByText('open', { exact: true }).first().click();
    await page.waitForURL(/opened=run1/, { timeout: TIMEOUT });
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// The runs route answers 503 (no cloud runner): one shared line says so.
async function testCloudRunsNotConfigured(browser, site) {
  site.state.runsOff = true;
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/', site.base);
  try {
    await openCloud(page);
    await page.waitForFunction(() => document.body.innerText.includes('the bridge has no cloud runner yet'), null, { timeout: TIMEOUT });
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
  }
}

// The vault (PLAN 10.9 item 3) on a live page: the API-key row reads the vault,
// a key goes to the bridge (PUT, never a session frame), removal DELETEs.
async function testVaultKeyRows(browser, site) {
  const ws = await startWsServer({ providers: [
    { id: 'anthropic', label: 'Anthropic', status: 'not configured', oauth_capable: true },
    { id: 'openai', label: 'OpenAI', status: 'not configured', oauth_capable: false },
  ] });
  const { context, page, errors } = await openIsolated(browser, site.base + '/app/?url=' + encodeURIComponent(ws.url) + '&session=' + SESSION, site.base);
  try {
    await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: TIMEOUT });
    await page.keyboard.type('/auth');
    await page.waitForFunction(() => document.activeElement.value === '/auth', null, { timeout: TIMEOUT });
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('the key is stored for your cloud hosts'), null, { timeout: TIMEOUT });
    await waitRow(page, 'OpenAI', 'not configured');
    await page.waitForTimeout(300);
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(100);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => document.body.innerText.includes('OpenAI: paste an API key'), null, { timeout: TIMEOUT });
    await typeMasked(page, 'sk-vault-1');
    await page.keyboard.press('Enter');
    await waitUntil(() => site.state.vaultPuts.length === 1, 'the vault PUT');
    if (!sameBody(site.state.vaultPuts[0], { id: 'openai', body: { key: 'sk-vault-1' } })) {
      throw new Error('PUT was ' + JSON.stringify(site.state.vaultPuts[0]));
    }
    if (ws.received.includes('auth_set_key')) throw new Error('the key went over the session socket');
    await waitRow(page, 'OpenAI', 'saved in the vault');
    if ((await page.content()).includes('sk-vault-1')) throw new Error('the key stayed on the page');
    await tapLabel(page, 'remove');
    await page.waitForFunction(() => document.body.innerText.includes('remove OpenAI credentials?'), null, { timeout: TIMEOUT });
    await tapLabel(page, 'y');
    await waitUntil(() => site.state.vaultDeletes.includes('openai'), 'the vault DELETE');
    if (ws.received.includes('auth_remove')) throw new Error('the removal went over the session socket');
    await waitRow(page, 'OpenAI', 'not configured');
    await assertNoSecrets(page);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await context.close();
    ws.close();
  }
}

if (require.main === module) (async () => {
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
    ['(e2) approval buttons ignore a tap in the first 300 ms of a prompt, then approve', () => testApprovalGuard(browser, base)],
    ['(f) theme primary colour applied on web', () => testThemeColour(browser, base)],
    ['(g) share link (no session in URL): submit is for the adopted session', () => testShareLinkSubmit(browser, base)],
    ['(h) queue chip press sends dequeue', () => testChipDequeue(browser, base)],
    ['(i) header controls open the sessions and settings screens', () => testHeaderOpensScreens(browser, base)],
    ['(l) theme picker: light and dark repaint, the dark choice survives a reload', () => testThemePicker(browser, base)],
    ['(j) session row tap sends attach', () => testSessionRowAttaches(browser, base)],
    ['(j2) V4: a forked session row names its parent and turn; /fork 2 sends turn 2', () => testForkRowAndTurn(browser, base)],
    ['(j3) Part B: listed project commands are in the popup and sent; an unlisted /word is a local notice', () => testProjectCommands(browser, base)],
    ['(k) effort tap sends set_config', () => testEffortTapSendsConfig(browser, base)],
    ['(l) typing /co shows the compact suggestion', () => testSlashSuggestions(browser, base)],
    ['(m) model picker row sends set_config with the model', () => testModelPickerSendsConfig(browser, base)],
    ['(n) delete asks inline, then the yes control sends delete_session', () => testDeleteAfterConfirmation(browser, base)],
    ['(t) no page or console error for a few seconds and across a forced reconnect', () => testNoErrorsAcrossReconnect(browser, base)],
    ['(t2) a dropped link reads reconnecting with a 44 px retry now that reconnects at once', () => testReconnectingBannerAndRetryNow(browser, base)],
    ['(t3) a blank web session: one welcome empty state, no terminal key hints', () => testWebWelcomeEmptyState(browser, base)],
    ['(s) credentials: /auth opens it, OAuth sends auth_start, a typed key is sent once and leaves the page', () => testCredentialsScreen(browser, base)],
    ['(s2) ChatGPT row: Enter starts the device flow; the URL and code show, then the outcome', () => testChatGptSignIn(browser, base)],
    ['(s2a) ChatGPT row: the api key button opens the masked entry, the sign-in button starts the device flow', () => testChatGptRowHasKeyAndSignInButtons(browser, base)],
    ['(s2) agent picker: /agent lists the server personas, a tap sends set_config with the persona', () => testPersonaPicker(browser, base)],
    ['(s2b) agent picker: the first row clears the persona with an empty set_config', () => testPersonaClear(browser, base)],
    ['(s3) credentials: remove on a signed-in row only, asks y / n, then sends auth_remove', () => testCredentialRemove(browser, base)],
    ['(s4) a confirmation is a modal dialog: no key hint, Tab stays inside, Escape cancels, y sends', () => testConfirmDialog(browser, base)],
    ['(o) agent screen: steer sends steer_agent, kill confirms then sends kill_agent', () => testAgentSteerAndKill(browser, base)],
    ['(q) shift+enter adds a line and does not submit', () => testShiftEnterNewline(browser, base)],
    ['(r) esc on a pending approval sends deny', () => testEscDeniesApproval(browser, base)],
    ['(w) mouse wheel over the transcript reaches the oldest row', () => testWheelScrollsTranscript(browser, base)],
    ['(l2) at 1280x720 and 390x844 every control and the composer are inside the window (approval, run, two queued)', () => testLayoutInViewport(browser, base)],
    ['(x) chat on a phone: send is 44 px, the composer is pinned at the bottom, no sideways scroll, approval words are buttons', () => testChatWeb(browser, base)],
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
    ['(d1b) "!!ls" sends a private shell frame (no submit)', () => testBangBangPrivate(browser, base)],
    ['(h) attach control: one image with the submit, then its chip goes', () => testAttachSendsOneImage(browser, base)],
    ['(i) Esc on an idle composer clears the typed text, sends nothing', () => testEscClearsDraft(browser, base)],
    ['(w1) push row hidden where the key route answers 503', async () => {
      const s = await portalSite('on'); try { await testPushHiddenOn503(browser, s); } finally { s.close(); } }],
    ['(w2) push row off on a new device; a tap subscribes and posts the exact body', async () => {
      const s = await portalSite('on'); try { await testPushTapSubscribes(browser, s); } finally { s.close(); } }],
    ['(w3) push: a denied permission leaves the row off and names the reason', async () => {
      const s = await portalSite('on'); try { await testPushDeniedStaysOff(browser, s); } finally { s.close(); } }],
    ['(w4) push: an existing subscription shows on; a second tap unsubscribes and DELETEs', async () => {
      const s = await portalSite('on'); try { await testPushSecondTapUnsubscribes(browser, s); } finally { s.close(); } }],
    ['(w5) push: a viewer invite hides the row', async () => {
      const s = await portalSite('on'); try { await testPushViewerInviteHidden(browser, s); } finally { s.close(); } }],
    ['(w6) push: an approver invite subscribes with its invite', async () => {
      const s = await portalSite('on'); try { await testPushApproverInvite(browser, s); } finally { s.close(); } }],
    ['(w7) push: an iPhone tab without push shows unavailable with the Home Screen hint', async () => {
      const s = await portalSite('on'); try { await testPushIosTabHint(browser, s); } finally { s.close(); } }],
    ['(c1) cloud: the entry is not drawn where the accounts route answers 503', async () => {
      const s = await portalSite('on'); try { await testCloudHiddenOn503(browser, s); } finally { s.close(); } }],
    ['(c2) cloud: a masked token is POSTed and not shown; a refused token shows GitHub status', async () => {
      const s = await portalSite('on'); try { await testCloudAddAccount(browser, s); } finally { s.close(); } }],
    ['(c3) cloud: remove asks y / n, then DELETEs the account', async () => {
      const s = await portalSite('on'); try { await testCloudRemoveAccount(browser, s); } finally { s.close(); } }],
    ['(c4) cloud: the repo picker sends its text as q once, and pages at the last row', async () => {
      const s = await portalSite('on'); try { await testCloudRepoPicker(browser, s); } finally { s.close(); } }],
    ['(c5) cloud: a pick fills the run; start posts {account, repo, branch} and the run shows', async () => {
      const s = await portalSite('on'); try { await testCloudStartRun(browser, s); } finally { s.close(); } }],
    ['(c6) cloud: stop, wake and open hit their routes; open goes to the session address', async () => {
      const s = await portalSite('on'); try { await testCloudRunActions(browser, s); } finally { s.close(); } }],
    ['(c7) cloud: runs answered 503 shows the shared not-configured line', async () => {
      const s = await portalSite('on'); try { await testCloudRunsNotConfigured(browser, s); } finally { s.close(); } }],
    ['(v1) vault: the key row reads the vault, a key is PUT and never a session frame, removal DELETEs', async () => {
      const s = await portalSite('on'); try { await testVaultKeyRows(browser, s); } finally { s.close(); } }],
    ['(u2) one socket per page load: live, invite ticket and owner pages open exactly one', async () => {
      const s = await portalSite('on'); try { await testOneSocketPerLoad(browser, s); } finally { s.close(); } }],
    ['(u3) invite ticket: a slow welcome does not open a second socket with the spent ticket', async () => {
      const s = await portalSite('on'); try { await testTicketSlowWelcome(browser, s); } finally { s.close(); } }],
    ['(u1) sign-in: the button is a 44 px target and the page does not scroll sideways', async () => {
      const s = await portalSite('on'); try { await testSignInLayout(browser, s); } finally { s.close(); } }],
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

// The helpers the screenshot tool (shots.mjs) reuses: the scripted site, the
// scripted bridge and the fixtures. Requiring this file runs nothing.
module.exports = { startSite, startWsServer, newSiteState, PORTAL_ROWS, SESSION, COOKIE, TOKEN, STREAM, freePort, openIsolated, tapLabel, TIMEOUT };
