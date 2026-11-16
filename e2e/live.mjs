// Packet LIVE-1: end-to-end checks against a DEPLOYED preview bridge.
//
//   E2E_LIVE_ENV=<env file with URL=, OWNER_TOKEN=, HOST_TOKEN=> node e2e/live.mjs
//
// Exits 0 with SKIP when E2E_LIVE_ENV is unset. Token values are never printed,
// written to the repo, or left in logs (every message passes through clean()).
//
// Transport stand-in: the Rust serve client (tokio-tungstenite with webpki
// roots, reqwest with rustls) does not trust the egress proxy CA that this
// container's HTTPS path re-signs with, and it does not read HTTPS_PROXY. So
// the serve dials a loopback forwarder (plain ws:// and http:// are allowed
// for loopback hosts by rc.rs), and the forwarder carries its agent tunnel
// and portal registrations to the deployed bridge with Node's ws and fetch.
// Everything else (browser, owner and invite traffic, the bridge itself)
// talks to the deployed URL directly.
import { readFileSync, appendFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { randomUUID, randomBytes } from 'node:crypto';
import * as L from './lib.mjs';

if (!process.env.E2E_LIVE_ENV) {
  console.log('SKIP live: E2E_LIVE_ENV is not set');
  process.exit(0);
}

const envFile = Object.fromEntries(
  readFileSync(process.env.E2E_LIVE_ENV, 'utf8')
    .split('\n')
    .filter((l) => l.includes('='))
    .map((l) => { const i = l.indexOf('='); return [l.slice(0, i).trim(), l.slice(i + 1).trim()]; }),
);
const BASE = (envFile.URL || '').replace(/\/+$/, '');
const OWNER = envFile.OWNER_TOKEN || '';
const HOST = envFile.HOST_TOKEN || '';
if (!BASE || !OWNER || !HOST) {
  console.log('FAIL live: the env file lacks URL, OWNER_TOKEN or HOST_TOKEN');
  process.exit(1);
}
const UP_WS = BASE.replace(/^https:/, 'wss:');
const RUN = Date.now().toString(36) + randomBytes(2).toString('hex');
const sleep = L.sleep;

// Never let a secret reach stdout or a report.
const clean = (s) => {
  let t = String(s);
  for (const secret of [OWNER, HOST]) if (secret) t = t.split(secret).join('<secret>');
  return t.replace(/\/agent\/[^\s'"]+/g, '/agent/<key>');
};

const require = createRequire(join(L.BRIDGE_DIR, 'package.json'));
const { WebSocketServer, WebSocket } = require('ws');
const { chromium } = createRequire(import.meta.url)('playwright');

// ---- loopback forwarder (serve side only) ---------------------------------

async function startForwarder() {
  const port = await L.freePort();
  const log = join(L.SCRATCH, 'forwarder.log');
  const note = (line) => appendFileSync(log, new Date().toISOString() + ' ' + clean(line) + '\n');
  const wss = new WebSocketServer({ noServer: true });
  const server = createServer(async (req, res) => {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    const body = Buffer.concat(chunks);
    const headers = {};
    for (const k of ['authorization', 'content-type']) if (req.headers[k]) headers[k] = req.headers[k];
    try {
      const up = await fetch(BASE + req.url, {
        method: req.method, headers, body: body.length ? body : undefined,
        redirect: 'manual', signal: AbortSignal.timeout(20000),
      });
      const buf = Buffer.from(await up.arrayBuffer());
      note(`http ${req.method} ${req.url.replace(/sessions\/[^?]*/, 'sessions/<id>')} -> ${up.status}`);
      res.writeHead(up.status, { 'content-type': up.headers.get('content-type') || 'application/octet-stream' });
      res.end(buf);
    } catch (e) {
      note(`http ${req.method} -> forwarder error ${e.cause ? e.cause.code || e.cause.message : e.message}`);
      res.writeHead(502);
      res.end('forwarder error');
    }
  });
  server.on('upgrade', (req, socket, head) => {
    if (!req.url.startsWith('/agent/')) { socket.destroy(); return; }
    wss.handleUpgrade(req, socket, head, (client) => {
      const headers = req.headers.authorization ? { authorization: req.headers.authorization } : {};
      const up = new WebSocket(UP_WS + req.url, { headers, maxPayload: 64 * 1024 * 1024 });
      const queue = [];
      up.on('unexpected-response', (_r, res) => { note(`ws agent upstream refused: HTTP ${res.statusCode}`); try { client.close(); } catch { /* gone */ } });
      up.on('open', () => { note('ws agent upstream open'); for (const [d, b] of queue.splice(0)) up.send(d, { binary: b }); });
      up.on('message', (d, b) => { if (client.readyState === WebSocket.OPEN) client.send(d, { binary: b }); });
      client.on('message', (d, b) => { if (up.readyState === WebSocket.OPEN) up.send(d, { binary: b }); else queue.push([d, b]); });
      up.on('close', (code) => { note(`ws agent upstream closed ${code}`); try { client.close(); } catch { /* gone */ } });
      client.on('close', () => { try { up.close(); } catch { /* gone */ } });
      up.on('error', (e) => { note('ws agent upstream error ' + (e.code || e.message)); try { client.close(); } catch { /* gone */ } });
      client.on('error', () => { try { up.close(); } catch { /* gone */ } });
    });
  });
  await new Promise((r) => server.listen(port, '127.0.0.1', r));
  return { server, ws: `ws://127.0.0.1:${port}`, log };
}

// ---- HTTP helpers against the deployed URL ---------------------------------

async function call(method, path, { cookie, bearer, body } = {}) {
  const headers = {};
  if (cookie) headers.cookie = cookie;
  if (bearer) headers.authorization = 'Bearer ' + bearer;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const url = path.startsWith('http') ? path : BASE + path;
  const r = await fetch(url, {
    method, headers, redirect: 'manual',
    body: body !== undefined ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(20000),
  });
  const buf = Buffer.from(await r.arrayBuffer());
  let json = null;
  try { json = JSON.parse(buf.toString('utf8')); } catch { /* not JSON */ }
  return { status: r.status, headers: r.headers, ct: r.headers.get('content-type') || '', json, text: buf.toString('utf8'), bytes: buf };
}

// POST /auth allows five attempts a minute per client IP: wait out a 429 once.
async function postAuth(token) {
  let r = null;
  for (let i = 0; i < 2; i++) {
    r = await call('POST', '/auth', { body: { token } });
    if (r.status !== 429) return r;
    await sleep(65000);
  }
  return r;
}

const cookieFrom = (r) => {
  const sc = r.headers.getSetCookie ? r.headers.getSetCookie() : [];
  const hit = sc.map((c) => c.split(';')[0]).find((c) => c.startsWith('modron_owner='));
  return hit || null;
};

async function waitListed(cookie, sid, ms = 60000) {
  return L.waitUntil(async () => {
    const r = await call('GET', '/api/sessions', { cookie });
    const row = Array.isArray(r.json) ? r.json.find((x) => x.id === sid) : null;
    return row && row.online ? row : null;
  }, ms, `session ${sid.slice(0, 8)} online in /api/sessions`);
}

// ---- scenario plumbing -----------------------------------------------------

function makeS(id) {
  return {
    id, pass: [], fails: [], viewers: [],
    check(label, cond, detail = '') {
      if (cond) this.pass.push(label);
      else this.fails.push(label + (detail ? ': ' + clean(detail).slice(0, 240) : ''));
      return !!cond;
    },
    async soft(label, p) {
      try { return await p; } catch (e) { this.fails.push(label + ': ' + clean(e.message).slice(0, 240)); return null; }
    },
  };
}

const summary = [];
async function scenario(id, title, fn) {
  const s = makeS(id);
  const t0 = Date.now();
  let timer;
  try {
    await Promise.race([
      fn(s),
      new Promise((_, rej) => { timer = setTimeout(() => rej(new Error('240 s ceiling')), 240000); }),
    ]);
  } catch (e) {
    s.fails.push('exception: ' + clean(e.message).slice(0, 240));
  } finally {
    clearTimeout(timer);
  }
  for (const v of s.viewers) for (const e of v.errors) s.fails.push(`console error [${v.label}]: ${clean(e).slice(0, 200)}`);
  const ok = s.fails.length === 0;
  summary.push({ id, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${id} ${title} (${((Date.now() - t0) / 1000).toFixed(0)} s)`);
  for (const p of s.pass) console.log(`  ok    ${p}`);
  for (const f of s.fails) console.log(`  FAIL  ${f}`);
}

const ctx = { cookie: null, fwd: null, browser: null, serves: [], registered: [], invites: [], sid4: null };
const V = (s, v) => { s.viewers.push(v); return v; };

async function startLive(name) {
  const srv = await L.startServe(name, { bridgeWs: ctx.fwd.ws, hostToken: HOST });
  ctx.serves.push(name);
  const lc = new L.Local(srv);
  await lc.connect();
  return { srv, lc, name };
}

async function startSession(name, title) {
  const sv = await startLive(name);
  const sid = await sv.lc.newSession(title);
  await sv.lc.attach(sid);
  ctx.registered.push(sid);
  await sv.lc.rc(sid, ctx.fwd.ws);
  return { ...sv, sid };
}

function cookieParts() {
  const i = ctx.cookie.indexOf('=');
  return { name: ctx.cookie.slice(0, i), value: ctx.cookie.slice(i + 1), url: BASE };
}

// ---- L1 static web app ------------------------------------------------------

async function L1(s) {
  const root = await call('GET', '/');
  s.check('GET / answers 302', root.status === 302, `status ${root.status}`);
  const loc = root.headers.get('location') || '';
  s.check('GET / redirects to /app/', new URL(loc, BASE + '/').pathname === '/app/', `location ${loc.slice(0, 60)}`);

  const app = await call('GET', '/app/');
  s.check('GET /app/ is 200', app.status === 200, `status ${app.status}`);
  s.check('/app/ is served as text/html', app.ct.startsWith('text/html'), app.ct);

  // Find the wasm asset: named in index.html, or in one of the scripts it loads.
  let wasmRef = (app.text.match(/[\w./-]*nsh[\w.-]*\.wasm/) || [])[0];
  if (!wasmRef) {
    const scripts = [...app.text.matchAll(/<script[^>]+src=["']([^"']+)["']/g)].map((m) => m[1]);
    for (const src of scripts) {
      const js = await call('GET', new URL(src, BASE + '/app/').href);
      const hit = (js.text.match(/[\w./-]*nsh[\w.-]*\.wasm/) || [])[0];
      if (hit) { wasmRef = hit; break; }
    }
  }
  s.check('the app references an nsh wasm asset', !!wasmRef, 'no .wasm name in index.html or its scripts');
  if (!wasmRef) return;
  const w = await call('GET', new URL(wasmRef, BASE + '/app/').href);
  s.check('wasm asset is 200', w.status === 200, `status ${w.status}`);
  s.check('wasm asset is application/wasm', w.ct.startsWith('application/wasm'), w.ct);
  const magic = w.bytes.subarray(0, 4).toString('hex');
  s.check('wasm asset starts with the wasm magic', magic === '0061736d', magic);
  const cc = w.headers.get('cache-control') || '';
  s.check('wasm asset has a cache header', cc.length > 0 && !/no-store/.test(cc), `cache-control "${cc}"`);
  console.log(`  info  wasm ${w.bytes.length} bytes, cache-control "${cc}"`);
}

// ---- L2 portal gate ---------------------------------------------------------

async function L2(s) {
  const me = await call('GET', '/api/me');
  s.check('/api/me without a cookie is refused (401)', me.status === 401, `status ${me.status}`);
  const bad = await postAuth('wrong-' + RUN);
  s.check('POST /auth with a wrong token is 401 and sets no cookie', bad.status === 401 && !cookieFrom(bad), `status ${bad.status}`);
  const good = await postAuth(OWNER);
  ctx.cookie = cookieFrom(good);
  s.check('POST /auth with the owner token is 200 and sets modron_owner', good.status === 200 && !!ctx.cookie, `status ${good.status}`);
  if (!ctx.cookie) return;
  const me2 = await call('GET', '/api/me', { cookie: ctx.cookie });
  s.check('/api/me with the cookie reports the owner', me2.status === 200 && me2.json && me2.json.owner === true, `status ${me2.status}`);
}

// ---- L3 host registration ---------------------------------------------------

async function L3(s) {
  if (!ctx.cookie) { s.fails.push('prerequisite L2 gave no owner cookie'); return; }
  const id = randomUUID();
  const refusedId = randomUUID();
  const body = {
    title: `e2e live ${RUN} L3`, host_label: 'e2e live probe', host_kind: 'local',
    viewer_key: randomBytes(16).toString('hex'), state: 'idle',
  };
  const none = await call('PUT', '/api/host/sessions/' + refusedId, { body });
  s.check('host PUT without a bearer is refused (401)', none.status === 401, `status ${none.status}`);
  const wrong = await call('PUT', '/api/host/sessions/' + refusedId, { bearer: HOST + 'x', body });
  s.check('host PUT with a wrong bearer is refused (401)', wrong.status === 401, `status ${wrong.status}`);
  const put = await call('PUT', '/api/host/sessions/' + id, { bearer: HOST, body });
  s.check('host PUT with HOST_TOKEN is 204', put.status === 204, `status ${put.status}`);
  ctx.registered.push(id);
  const row = await s.soft('registered session listed for the owner', waitListed(ctx.cookie, id, 30000));
  if (row) {
    s.check('listed row carries the title and online=true', row.title === body.title && row.online === true, JSON.stringify(row).slice(0, 200));
  }
  const list = await call('GET', '/api/sessions', { cookie: ctx.cookie });
  s.check('refused PUT did not create a row', Array.isArray(list.json) && !list.json.some((x) => x.id === refusedId));
  const del = await call('DELETE', '/api/host/sessions/' + id, { bearer: HOST });
  s.check('host DELETE with HOST_TOKEN is 204', del.status === 204, `status ${del.status}`);
  ctx.registered = ctx.registered.filter((x) => x !== id);
}

// ---- L4 the real chain ------------------------------------------------------

async function L4(s) {
  if (!ctx.cookie) { s.fails.push('prerequisite L2 gave no owner cookie'); return; }
  const title = `e2e live ${RUN} L4`;
  const sv = await s.soft('start serve and remote control', startSession('l4', title));
  if (!sv) return;
  ctx.sid4 = sv.sid;
  const row = await s.soft('serve registered online with the portal', waitListed(ctx.cookie, sv.sid, 60000));
  if (!row) return;

  const owner = V(s, await L.openViewer(ctx.browser, BASE + '/', 'owner-l4'));
  s.check('GET / lands on /app/', owner.finalUrl.includes('/app/'), owner.finalUrl);
  await s.soft('sign-in screen', L.waitBody(owner, (t) => t.includes('sign in'), 30000, 'sign-in screen'));
  await s.soft('owner token box', owner.page.getByPlaceholder('owner token').fill(OWNER, { timeout: 15000 }));
  await s.soft('sign in button', owner.page.getByRole('button', { name: 'sign in' }).click({ timeout: 15000 }));
  await s.soft('session row online in the list', L.waitBody(owner, (t) => t.includes(title) && t.includes('online'), 40000, 'row online'));
  await s.soft('open the session from the list', L.clickContains(owner, title)
    .then(() => L.waitBody(owner, (t) => t.includes('connected'), 30000, 'connected')));

  const from = sv.lc.frames.length;
  const task = `e2e live ${RUN} L4 task`;
  await s.soft('owner types a task', L.typeAndSend(owner, task));
  const req = await s.soft('approval reached the serve', sv.lc.waitFor(
    (f) => f.type === 'approval_requested' && f.session === sv.sid, 60000, 'approval_requested', from));
  await s.soft('approval strip in the browser', L.waitBody(owner, (t) => t.includes('allow once'), 30000, 'allow once'));
  await s.soft('allow once tapped in the browser', L.clickText(owner, 'allow once'));
  if (req) {
    await s.soft('approval resolved by the browser', sv.lc.waitFor(
      (f) => f.type === 'approval_resolved' && f.request_id === req.request_id, 20000, 'approval_resolved', from));
  }
  s.check('browser sent an allow_once frame', L.wsFrames(owner, 'out', 'allow_once').length > 0, 'no allow_once frame from the browser socket');
  sv.lc.setAuto(true); // the mock's later write and bash steps
  await s.soft('run finished', sv.lc.turnDone(sv.sid, 120000, from));
  await s.soft('streamed mock answer in the browser', L.waitBody(owner, (t) => t.includes('Demo complete.'), 30000, 'mock answer'));
  s.check('task echo shown in the browser', (await L.bodyText(owner)).includes(task), 'task text not on the page');
}

// ---- L5 invites -------------------------------------------------------------

async function L5(s) {
  if (!ctx.cookie) { s.fails.push('prerequisite L2 gave no owner cookie'); return; }
  const title = `e2e live ${RUN} L5`;
  const sv = await s.soft('start serve and remote control', startSession('l5', title));
  if (!sv) return;
  const row = await s.soft('serve registered online', waitListed(ctx.cookie, sv.sid, 60000));
  if (!row) return;

  const mint = async (role) => {
    const r = await call('POST', '/api/invites', { cookie: ctx.cookie, body: { session_id: sv.sid, role, ttl_hours: 1 } });
    s.check(`owner mints a ${role} invite (201)`, r.status === 201 && r.json && r.json.url, `status ${r.status}`);
    if (r.status !== 201 || !r.json) return null;
    ctx.invites.push(r.json.id);
    return new URL(r.json.url);
  };
  const vUrl = await mint('viewer');
  const aUrl = await mint('approver');
  if (!vUrl || !aUrl) return;

  const guest = V(s, await L.openViewer(ctx.browser, vUrl.href, 'viewer-l5')); // fresh context, no cookies
  s.check('viewer link lands in the app', guest.finalUrl.includes('/app/?url='), guest.finalUrl);
  await s.soft('viewer sees connected', L.waitBody(guest, (t) => t.includes('connected'), 30000, 'connected'));
  const probe = `e2e live ${RUN} viewer try`;
  await s.soft('viewer types a submit', L.typeAndSend(guest, probe));
  await s.soft('viewer submit answered forbidden', L.waitUntil(
    () => L.wsFrames(guest, 'in', 'forbidden').length > 0, 15000, 'forbidden reply'));
  await sleep(1500);
  s.check('viewer submit did not start a run', !sv.lc.saw(probe), 'the serve echoed the viewer text');

  const approver = V(s, await L.openViewer(ctx.browser, aUrl.href, 'approver-l5'));
  s.check('approver link lands in the app', approver.finalUrl.includes('/app/?url='), approver.finalUrl);
  await s.soft('approver sees connected', L.waitBody(approver, (t) => t.includes('connected'), 30000, 'connected'));

  const from = sv.lc.frames.length;
  await sv.lc.run(`e2e live ${RUN} L5 task`, sv.sid);
  const req = await s.soft('approval reached the serve', sv.lc.waitFor(
    (f) => f.type === 'approval_requested' && f.session === sv.sid, 60000, 'approval_requested', from));
  await s.soft('approver sees the approval', L.waitBody(approver, (t) => t.includes('allow once'), 30000, 'allow once'));
  await s.soft('approver taps allow once', L.clickText(approver, 'allow once'));
  if (req) {
    await s.soft('approval resolved by the approver', sv.lc.waitFor(
      (f) => f.type === 'approval_resolved' && f.request_id === req.request_id, 20000, 'approval_resolved', from));
  }
  s.check('approver sent an allow_once frame', L.wsFrames(approver, 'out', 'allow_once').length > 0, 'no allow_once frame from the approver socket');
  sv.lc.setAuto(true);
  await s.soft('run finished', sv.lc.turnDone(sv.sid, 120000, from));
}

// ---- L6 reconnect -----------------------------------------------------------

async function L6(s) {
  if (!ctx.cookie) { s.fails.push('prerequisite L2 gave no owner cookie'); return; }
  const title = `e2e live ${RUN} L6`;
  const sv = await s.soft('start serve and remote control', startSession('l6', title));
  if (!sv) return;
  const row = await s.soft('serve registered online', waitListed(ctx.cookie, sv.sid, 60000));
  if (!row) return;

  const owner = V(s, await L.openViewer(ctx.browser, BASE + '/', 'owner-l6', cookieParts()));
  await s.soft('owner opens the session', L.waitBody(owner, (t) => t.includes(title) && t.includes('online'), 40000, 'row online')
    .then(() => L.clickContains(owner, title))
    .then(() => L.waitBody(owner, (t) => t.includes('connected'), 30000, 'connected')));

  const task = `e2e live ${RUN} L6 reload`;
  const from = sv.lc.frames.length;
  await sv.lc.run(task, sv.sid);
  const req = await s.soft('approval reached the serve', sv.lc.waitFor(
    (f) => f.type === 'approval_requested' && f.session === sv.sid, 60000, 'approval_requested', from));
  await s.soft('approval shown before reload', L.waitBody(owner, (t) => t.includes('allow once'), 30000, 'allow once'));
  await s.soft('reload mid-session', owner.page.reload({ waitUntil: 'domcontentloaded', timeout: 30000 }));
  await s.soft('transcript replayed after reload', L.waitBody(owner, (t) => t.includes(task) && t.includes('connected'), 40000, 'replayed transcript'));
  await s.soft('approval shown again after reload', L.waitBody(owner, (t) => t.includes('allow once'), 30000, 'allow once after reload'));
  await s.soft('allow once after reload', L.clickText(owner, 'allow once'));
  if (req) {
    await s.soft('approval resolved after reload', sv.lc.waitFor(
      (f) => f.type === 'approval_resolved' && f.request_id === req.request_id, 20000, 'approval_resolved', from));
  }
  sv.lc.setAuto(true);
  await s.soft('run finished', sv.lc.turnDone(sv.sid, 120000, from));
}

// ---- main -------------------------------------------------------------------

const t0 = Date.now();
ctx.fwd = await startForwarder();
console.log(`info  run ${RUN}; forwarder log ${ctx.fwd.log}`);
ctx.browser = await chromium.launch({
  headless: true,
  args: ['--no-sandbox', `--${L.RUN_TAG}`],
  ...(process.env.HTTPS_PROXY ? { proxy: { server: process.env.HTTPS_PROXY } } : {}),
});

try {
  await scenario('L1', 'web app: / redirects, /app/ serves, wasm MIME and cache', L1);
  await scenario('L2', 'portal gate: /api/me refused, owner sign-in, wrong token refused', L2);
  await scenario('L3', 'host registration: bearer required, listed for the owner', L3);
  await scenario('L4', 'real chain: local serve to deployed bridge, browser task and approval', L4);
  await scenario('L5', 'invites: viewer watches and is refused a submit, approver approves', L5);
  await scenario('L6', 'reconnect: reload mid-session replays the transcript', L6);
} finally {
  // Cleanup: revoke invites, remove the host registrations, stop serves and Chromium.
  const cleanupNotes = [];
  let inv = 0;
  for (const id of ctx.invites) {
    const r = await call('DELETE', '/api/invites/' + id, { cookie: ctx.cookie }).catch(() => ({ status: 0 }));
    if (r.status === 204) inv++;
  }
  cleanupNotes.push(`invites revoked ${inv}/${ctx.invites.length}`);
  let reg = 0;
  for (const sid of ctx.registered) {
    const r = await call('DELETE', '/api/host/sessions/' + sid, { bearer: HOST }).catch(() => ({ status: 0 }));
    if (r.status === 204) reg++;
  }
  cleanupNotes.push(`registrations removed ${reg}/${ctx.registered.length}`);
  for (const name of ctx.serves) await L.stopProc('serve-' + name).catch(() => {});
  await ctx.browser.close().catch(() => {});
  ctx.fwd.server.close();
  L.killEverything('SIGTERM');
  await sleep(500);
  L.killEverything('SIGKILL');
  cleanupNotes.push(`leftover chromium ${L.taggedCount()}, tracked processes ${L.liveCount()}`);
  console.log('cleanup ' + cleanupNotes.join('; '));
}

const fails = summary.filter((x) => !x.ok).length;
console.log(`SUMMARY live: ${summary.length - fails} passed, ${fails} failed (${((Date.now() - t0) / 1000).toFixed(0)} s)`);
process.exit(fails ? 1 : 0);
