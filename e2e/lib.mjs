// Helpers for e2e/run.mjs: tracked child processes (killed on exit), free
// ports, a minimal local protocol client (Node's built-in WebSocket), and
// Playwright viewer helpers. No npm installs; Playwright is preinstalled.
import { spawn } from 'node:child_process';
import { createConnection, createServer } from 'node:net';
import { cpSync, mkdirSync, mkdtempSync, openSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';

const require = createRequire(import.meta.url);

export const REPO = '/home/user/modron';
export const BRIDGE_DIR = join(REPO, 'cloud/bridge');
export const WRANGLER = join(BRIDGE_DIR, 'node_modules/.bin/wrangler');
export const MODRON = '/tmp/claude-0/e2e/modron';
export const APP_SRC = '/tmp/claude-0/e2e/app';
// Short scratch root: the modron Unix socket path must stay under 108 bytes.
export const SCRATCH = mkdtempSync('/tmp/claude-0/e2e/run-');

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- processes -----------------------------------------------------------

const procs = [];

function spawnLogged(name, cmd, args, { cwd, env, log }) {
  const fd = openSync(log, 'w');
  // detached => own process group, so workerd grandchildren die with it.
  const child = spawn(cmd, args, { cwd, env, stdio: ['ignore', fd, fd], detached: true });
  procs.push({ name, child });
  return child;
}

export function killAll(signal = 'SIGTERM') {
  for (const { child } of procs) {
    if (child.exitCode === null && child.signalCode === null) {
      try { process.kill(-child.pid, signal); } catch { /* already gone */ }
    }
  }
}

// Stop one tracked process (by its spawn name, e.g. 'serve-pb') and wait for it.
export async function stopProc(name, signal = 'SIGTERM', ms = 8000) {
  const p = procs.find((x) => x.name === name);
  if (!p) return false;
  if (p.child.exitCode === null && p.child.signalCode === null) {
    try { process.kill(-p.child.pid, signal); } catch { /* already gone */ }
  }
  await waitUntil(() => p.child.exitCode !== null || p.child.signalCode !== null, ms, `${name} exit`).catch(() => {});
  return true;
}

export function liveCount() {
  return procs.filter(({ child }) => child.exitCode === null && child.signalCode === null).length;
}

process.on('exit', () => killAll('SIGKILL'));
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(sig, () => { killAll('SIGKILL'); process.exit(130); });
}

export function tail(path, n = 30) {
  try {
    return readFileSync(path, 'utf8').split('\n').slice(-n).map((l) => l.slice(0, 300)).join('\n');
  } catch {
    return '(no log)';
  }
}

// ---- ports and waiting ----------------------------------------------------

export function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

export function portOpen(port) {
  return new Promise((resolve) => {
    const s = createConnection({ port, host: '127.0.0.1' });
    s.once('connect', () => { s.destroy(); resolve(true); });
    s.once('error', () => resolve(false));
  });
}

export async function waitUntil(fn, ms, label) {
  const end = Date.now() + ms;
  let last = null;
  while (Date.now() < end) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(250);
  }
  throw new Error(`timeout: ${label}${last ? ' (last error: ' + last.message + ')' : ''}`);
}

// ---- bridge (wrangler dev --local) ----------------------------------------

export async function startBridge(name, vars = {}) {
  const port = await freePort();
  const args = [
    'dev', '--local', '--ip', '127.0.0.1', '--port', String(port),
    '--show-interactive-dev-session=false', '--persist-to', join(SCRATCH, 'state-' + name),
  ];
  for (const [k, v] of Object.entries(vars)) args.push('--var', `${k}:${v}`);
  const log = join(SCRATCH, `bridge-${name}.log`);
  spawnLogged('bridge-' + name, WRANGLER, args, {
    cwd: BRIDGE_DIR,
    env: { ...process.env, WRANGLER_SEND_METRICS: 'false', CI: '1' },
    log,
  });
  const base = `http://127.0.0.1:${port}`;
  await waitUntil(async () => (await fetch(base + '/')).ok, 150000, `bridge ${name} ready (log ${log})`);
  return { name, port, base, ws: `ws://127.0.0.1:${port}`, log };
}

export function copyApp() {
  const dst = join(BRIDGE_DIR, 'public/app');
  rmSync(dst, { recursive: true, force: true });
  mkdirSync(dst, { recursive: true });
  cpSync(APP_SRC, dst, { recursive: true });
}

// ---- modron serve ----------------------------------------------------------

export async function startServe(name, { bridgeWs, hostToken } = {}) {
  const home = join(SCRATCH, 'h-' + name);
  const work = join(home, 'w');
  mkdirSync(join(work, '.modron'), { recursive: true }); // remote-control needs a project marker
  const port = await freePort();
  const env = { ...process.env, HOME: home };
  delete env.MODRON_HOST_TOKEN;
  if (hostToken) env.MODRON_HOST_TOKEN = hostToken;
  const log = join(SCRATCH, `serve-${name}.log`);
  const args = ['serve', '--listen', `127.0.0.1:${port}`, '--provider', 'mock', '--working-dir', work];
  if (bridgeWs) args.push('--bridge', bridgeWs);
  spawnLogged('serve-' + name, MODRON, args, { cwd: work, env, log });
  await waitUntil(() => portOpen(port), 60000, `serve ${name} listening (log ${log})`);
  return { name, port, ws: `ws://127.0.0.1:${port}`, log };
}

// ---- local protocol client (the test drives serve like a TUI would) -------

export class Local {
  constructor(serve) {
    this.serve = serve;
    this.frames = [];
    this.auto = false;
    this.pending = new Map(); // request_id -> frame, unresolved approvals
    this.nextId = 100;
  }

  async connect() {
    this.ws = new WebSocket(this.serve.ws);
    this.ws.onmessage = (e) => this.onFrame(JSON.parse(String(e.data)));
    await new Promise((res, rej) => {
      this.ws.onopen = res;
      this.ws.onerror = () => rej(new Error('local ws failed: ' + this.serve.ws));
    });
    this.send({ type: 'hello', client: 'e2e-local', protocol: 1, token: null });
    await this.waitFor((f) => f.type === 'welcome', 15000, 'welcome');
    return this;
  }

  onFrame(f) {
    this.frames.push(f);
    if (f.type === 'approval_requested') {
      this.pending.set(f.request_id, f);
      if (this.auto) this.approve(f.request_id);
    }
    if (f.type === 'approval_resolved') this.pending.delete(f.request_id);
  }

  setAuto(on) {
    this.auto = on;
    if (on) for (const id of [...this.pending.keys()]) this.approve(id);
  }

  approve(requestId) {
    this.send({ type: 'approve', request_id: requestId, decision: 'allow_once' });
  }

  send(obj) {
    const frame = { id: this.nextId++, ...obj };
    this.ws.send(JSON.stringify(frame));
    return frame.id;
  }

  waitFor(pred, ms = 60000, label = 'frame', from = 0) {
    return waitUntil(() => {
      for (let i = from; i < this.frames.length; i++) if (pred(this.frames[i])) return this.frames[i];
      return null;
    }, ms, label).catch((e) => {
      const recent = this.frames.slice(-8).map((f) => f.type).join(',');
      throw new Error(`${e.message}; recent frames: ${recent}`);
    });
  }

  // Submit a task. session = null starts a new session; returns the session id.
  async run(task, session = null) {
    const from = this.frames.length;
    this.send({ type: 'submit', session_id: session, task });
    if (session) return session;
    const f = await this.waitFor((x) => x.session, 20000, 'session id from submit', from);
    await this.attach(f.session);
    return f.session;
  }

  async attach(sid) {
    const from = this.frames.length;
    this.send({ type: 'attach', session_id: sid, last_seq: 0 });
    return this.waitFor((f) => f.type === 'snapshot' && f.session_id === sid, 20000, 'snapshot', from);
  }

  async rc(sid, bridgeWs) {
    const from = this.frames.length;
    this.send({ type: 'remote_control', session_id: sid, bridge_url: bridgeWs });
    const f = await this.waitFor(
      (x) => x.type === 'remote_control_started' || x.type === 'error', 20000, 'remote_control_started', from,
    );
    if (f.type === 'error') throw new Error('remote_control refused: ' + f.message);
    return f.url;
  }

  async sessions() {
    const from = this.frames.length;
    this.send({ type: 'list_sessions' });
    return (await this.waitFor((x) => x.type === 'sessions', 15000, 'sessions', from)).sessions;
  }

  turnDone(sid, ms = 90000, from = 0) {
    return this.waitFor((f) => f.type === 'turn_done' && f.session === sid, ms, 'turn_done', from);
  }

  // Creates an empty session (no run, so the mock script is not consumed).
  // The reply is a note with the id in the frame's session field.
  async newSession(title) {
    const from = this.frames.length;
    this.send({ type: 'new_session', title });
    const f = await this.waitFor(
      (x) => x.type === 'note' && String(x.text).startsWith('session created: '), 15000, 'session created', from,
    );
    return f.session;
  }

  // Frames after `from` whose JSON contains needle.
  saw(needle, from = 0) {
    return this.frames.slice(from).some((f) => JSON.stringify(f).includes(needle));
  }

  close() {
    try { this.ws.close(); } catch { /* ignore */ }
  }
}

// ---- browser (Playwright, headless Chromium) -------------------------------

export async function launchBrowser() {
  const { chromium } = require('playwright');
  return chromium.launch({ headless: true, args: ['--no-sandbox'] });
}

// `cookie` ({ name, value, url }) is set in the new context before the page
// loads: an owner browser for the owner-only steps (config, shell, steer).
export async function openViewer(browser, url, label, cookie = null) {
  const ctx = await browser.newContext();
  if (cookie) await ctx.addCookies([{ name: cookie.name, value: cookie.value, url: cookie.url }]);
  const page = await ctx.newPage();
  const v = { label, ctx, page, sockets: [], errors: [], finalUrl: '', requested: url };
  page.on('websocket', (sock) => {
    const rec = { url: sock.url(), frames: [] };
    v.sockets.push(rec);
    sock.on('framereceived', (e) => rec.frames.push(['in', String(e.payload)]));
    sock.on('framesent', (e) => rec.frames.push(['out', String(e.payload)]));
    sock.on('close', () => rec.frames.push(['close', '']));
  });
  // The app asks GET /api/me on every page to learn whether it is the
  // portal owner. A relay without a portal answers 503 and a guest gets
  // 401; the browser logs both as resource errors. Those two are
  // expected; every other console error still fails the step.
  const expected = /server responded with a status of (401|503)/;
  page.on('console', (m) => {
    if (m.type() === 'error' && !expected.test(m.text())) v.errors.push(m.text());
  });
  page.on('pageerror', (e) => v.errors.push('pageerror: ' + e.message));
  const resp = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 }).catch((e) => { v.errors.push('goto: ' + e.message); return null; });
  v.status = resp ? resp.status() : null; // main-document status (a refused invite is 410)
  v.finalUrl = page.url();
  return v;
}

// page.evaluate has no timeout of its own: a wedged page would hang the run,
// so a 5 s race turns a hang into a marker the checks can report.
export async function bodyText(v) {
  const read = v.page.evaluate(() => (document.body ? document.body.innerText : '')).catch(() => '');
  const stall = sleep(5000).then(() => '(page did not answer evaluate within 5 s)');
  return Promise.race([read, stall]);
}

export function waitBody(v, pred, ms = 30000, label = 'page text') {
  return waitUntil(async () => {
    const t = await bodyText(v);
    return pred(t) ? t : null;
  }, ms, `${v.label}: ${label}`);
}

export async function clickText(v, text) {
  await v.page.getByText(text, { exact: true }).first().click({ timeout: 15000 });
}

export async function typeAndSend(v, text) {
  const box = v.page.getByPlaceholder('Message').first();
  await box.click({ timeout: 15000 });
  await box.fill(text);
  await box.press('Enter');
}

export async function shot(v, name) {
  await v.page.screenshot({ path: join(SCRATCH, `${name || v.label}.png`) }).catch(() => {});
}

// Frames this viewer's socket received (dir 'in') or sent ('out') that contain needle.
export function wsFrames(v, dir, needle) {
  return v.sockets.flatMap((s) => s.frames).filter(([d, p]) => d === dir && p.includes(needle));
}

// Resolves true when the socket is refused or closes before opening.
export function refused(url, ms = 8000) {
  return new Promise((resolve) => {
    let ws;
    try { ws = new WebSocket(url); } catch { resolve(true); return; }
    const timer = setTimeout(() => { try { ws.close(); } catch { /* ignore */ } resolve(false); }, ms);
    ws.onopen = () => { clearTimeout(timer); ws.close(); resolve(false); };
    ws.onerror = () => { clearTimeout(timer); resolve(true); };
    ws.onclose = () => { clearTimeout(timer); resolve(true); };
  });
}

// ---- portal HTTP ------------------------------------------------------------

export async function ownerCookie(base, token) {
  const r = await fetch(base + '/auth', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ token }),
  });
  if (r.status !== 200) throw new Error('POST /auth returned ' + r.status);
  return r.headers.getSetCookie()[0].split(';')[0];
}

export async function api(base, cookie, method, path, body) {
  const r = await fetch(base + path, {
    method,
    headers: { cookie, 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await r.text();
  let json = text;
  try { json = JSON.parse(text); } catch { /* not JSON */ }
  return { status: r.status, json };
}

// Share URLs come back as ws(s)://; a browser needs http(s)://.
export function toHttp(url) {
  return url.replace(/^wss:/, 'https:').replace(/^ws:/, 'http:');
}

// ---- browser helpers for the portal and workflow scenarios -----------------

// Visible text of the page, role=button elements (their text) and the
// console-error list, for failure evidence.
export async function snapshotViewer(v) {
  const text = await bodyText(v);
  const buttons = await v.page
    .$$eval('[role=button]', (els) => els.map((e) => (e.innerText || e.textContent || '').trim()).slice(0, 80))
    .catch(() => ['(could not list buttons)']);
  return { label: v.label, url: v.page.url(), text: text.slice(0, 4000), buttons, errors: v.errors.slice(-10) };
}

// The portal's session list, read with the owner cookie from the browser context.
export async function ownerCookieFrom(v, base) {
  const cs = await v.ctx.cookies(base);
  const c = cs.find((x) => x.name === 'modron_owner');
  return c ? `${c.name}=${c.value}` : null;
}

// Click the first element whose text contains `text` (portal rows carry
// "title · host · state" in one line).
export async function clickContains(v, text, ms = 15000) {
  await v.page.getByText(text, { exact: false }).first().click({ timeout: ms });
}

// True while the approval strip shows its allow-once option.
export function approvalPending(v) {
  return bodyText(v).then((t) => t.includes('allow once'));
}

export function inviteUrlIn(text, base) {
  const m = text.match(/https?:\/\/[^\s]+\/i\/[A-Za-z0-9._-]+|\/i\/[A-Za-z0-9._-]+/);
  return m ? new URL(m[0], base) : null;
}

// Normalise ANSI-free terminal text for loose matching (cell redraws drop spaces).
export function squash(s) {
  return s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '').replace(/[^A-Za-z0-9]/g, '').toLowerCase();
}

// Newest mtime under a directory (recursive); used to decide whether a
// prebuilt binary is fresh relative to its sources.
export function newestMtime(dir) {
  let newest = 0;
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = statSync(p);
    newest = Math.max(newest, st.isDirectory() ? newestMtime(p) : st.mtimeMs);
  }
  return newest;
}
