// Helpers for e2e/run.mjs: tracked child processes (killed on exit), free
// ports, a minimal local protocol client (Node's built-in WebSocket), and
// Playwright viewer helpers. No npm installs; Playwright is preinstalled.
import { spawn, spawnSync } from 'node:child_process';
import { createServer as createHttpServer } from 'node:http';
import { createConnection, createServer } from 'node:net';
import { cpSync, mkdirSync, mkdtempSync, openSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);

// Paths: the repository is this file's parent; the prebuilt server and web
// bundle default to this machine's staging directory and can be pointed
// elsewhere (CI) with E2E_MODRON, E2E_APP_SRC and E2E_SCRATCH.
export const REPO = process.env.E2E_REPO || dirname(dirname(fileURLToPath(import.meta.url)));
export const BRIDGE_DIR = join(REPO, 'cloud/bridge');
export const WRANGLER = join(BRIDGE_DIR, 'node_modules/.bin/wrangler');
const STAGE = process.env.E2E_SCRATCH || '/tmp/claude-0/e2e';
mkdirSync(STAGE, { recursive: true });
export const MODRON = process.env.E2E_MODRON || join(STAGE, 'modron');
export const APP_SRC = process.env.E2E_APP_SRC || join(STAGE, 'app');
// Short scratch root: the modron Unix socket path must stay under 108 bytes.
export const SCRATCH = mkdtempSync(join(STAGE, 'run-'));

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

// Every Chromium this run starts carries RUN_TAG on its command line (see
// launchBrowser). Playwright's browser is not one of `procs`, so it is found
// by the tag: killed with the same signal as the tracked groups.
export const RUN_TAG = `modron-e2e-run-${process.pid}`;

export function killTagged(signal = 'SIGKILL') {
  spawnSync('pkill', [`-${signal.replace(/^SIG/, '')}`, '-f', RUN_TAG], { stdio: 'ignore' });
}

// Everything this run started: the tracked process groups and the tagged
// Chromium. Called from the exit handler, the signal handlers, and the
// wrap-up's finally.
export function killEverything(signal = 'SIGTERM') {
  killAll(signal);
  killTagged(signal);
}

export function taggedCount() {
  const r = spawnSync('pgrep', ['-f', RUN_TAG], { encoding: 'utf8' });
  return r.stdout ? r.stdout.trim().split('\n').filter(Boolean).length : 0;
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

process.on('exit', () => killEverything('SIGKILL'));
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(sig, () => { killEverything('SIGKILL'); process.exit(130); });
}
// An error outside a step (or a rejected promise) still tears everything down:
// 'exit' runs the group kill, and the exit code says the run did not finish.
process.on('uncaughtException', (e) => { console.error('uncaught: ' + (e && e.stack || e)); process.exit(1); });
process.on('unhandledRejection', (e) => { console.error('unhandled: ' + (e && e.stack || e)); process.exit(1); });

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

// remoteControl: a bridge ws(s) URL; the serve opens its tunnel at start
// (--remote-control) and writes the share URL to shareUrlFile (returned).
// provider (default mock) and portalCredentials (--portal-credentials) are for
// the vault scenario (C1). For a real provider the serve's environment drops
// OPENAI_API_KEY, so only the vault can supply it. extraEnv is merged last.
// runId: --run-id (a cloud run's serve, PLAN 10.10): its portal session carries the id.
export async function startServe(name, { bridgeWs, hostToken, config, remoteControl, provider = 'mock', portalCredentials = false, runId, extraEnv = {} } = {}) {
  const home = join(SCRATCH, 'h-' + name);
  const work = join(home, 'w');
  mkdirSync(join(work, '.modron'), { recursive: true }); // remote-control needs a project marker
  if (config) {
    // The serve reads ~/.config/modron/config.toml under this run's own HOME.
    mkdirSync(join(home, '.config', 'modron'), { recursive: true });
    writeFileSync(join(home, '.config', 'modron', 'config.toml'), config);
  }
  const port = await freePort();
  const env = { ...process.env, HOME: home };
  delete env.MODRON_HOST_TOKEN;
  if (hostToken) env.MODRON_HOST_TOKEN = hostToken;
  if (provider !== 'mock') delete env.OPENAI_API_KEY;
  Object.assign(env, extraEnv);
  const log = join(SCRATCH, `serve-${name}.log`);
  const args = ['serve', '--listen', `127.0.0.1:${port}`, '--provider', provider, '--working-dir', work];
  if (bridgeWs) args.push('--bridge', bridgeWs);
  const shareUrlFile = remoteControl ? join(home, 'share-url') : null;
  if (remoteControl) args.push('--remote-control', remoteControl, '--share-url-file', shareUrlFile);
  if (portalCredentials) args.push('--portal-credentials');
  if (runId) args.push('--run-id', runId);
  const child = spawnLogged('serve-' + name, MODRON, args, { cwd: work, env, log });
  await waitUntil(() => portOpen(port), 60000, `serve ${name} listening (log ${log})`);
  return { name, port, ws: `ws://127.0.0.1:${port}`, log, shareUrlFile, home, pid: child.pid };
}

// The cloud scenario's two fakes on 127.0.0.1 (PLAN 10.10): GitHub (GET /user,
// GET /user/repos, one repo) and the container worker (POST /wake, /stop).
// Every call is recorded; the wake body carries the git token, by design.
export async function startFakeCloud({ ghToken, wakeToken, login = 'modron-e2e' }) {
  const wakes = [];
  const json = (res, status, value) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(value)); };
  const gh = createHttpServer((req, res) => {
    if ((req.headers.authorization || '') !== `Bearer ${ghToken}`) return json(res, 401, { message: 'Bad credentials' });
    const url = new URL(req.url, 'http://127.0.0.1');
    if (url.pathname === '/user') return json(res, 200, { login });
    if (url.pathname === '/user/repos') {
      const page = Number(url.searchParams.get('page') || '1');
      const repo = { full_name: `${login}/app`, private: false, default_branch: 'main', pushed_at: new Date().toISOString() };
      return json(res, 200, page === 1 ? [repo] : []);
    }
    return json(res, 404, { message: 'Not Found' });
  });
  const ct = createHttpServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => { body += chunk; });
    req.on('end', () => {
      const parsed = body ? JSON.parse(body) : null;
      wakes.push({ path: req.url, auth: req.headers.authorization || '', body: parsed });
      if ((req.headers.authorization || '') !== `Bearer ${wakeToken}`) return json(res, 401, { error: 'unauthorized' });
      if (req.url === '/wake') return json(res, 202, { running: true });
      if (req.url === '/stop') return json(res, 200, { running: false });
      return json(res, 404, { error: 'not found' });
    });
  });
  for (const srv of [gh, ct]) await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  return {
    gh: `http://127.0.0.1:${gh.address().port}`,
    ct: `http://127.0.0.1:${ct.address().port}`,
    wakes,
    close: () => { gh.close(); ct.close(); },
  };
}

// A fake OpenAI-compatible endpoint on 127.0.0.1. Every request's path and
// Authorization header is recorded; it answers a minimal chat completion
// (plain JSON, or SSE when the request asks to stream).
export async function startFakeOpenAi() {
  const requests = [];
  const server = createHttpServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => { body += chunk; });
    req.on('end', () => {
      requests.push({ path: req.url, auth: req.headers.authorization || '' });
      let stream = false;
      try { stream = JSON.parse(body).stream === true; } catch { /* not JSON */ }
      const usage = { prompt_tokens: 1, completion_tokens: 2, total_tokens: 3 };
      if (stream) {
        const text = { id: 'e2e-fake', object: 'chat.completion.chunk', created: 0, model: 'e2e-fake',
          choices: [{ index: 0, delta: { role: 'assistant', content: 'vault ok' }, finish_reason: null }] };
        const last = { ...text, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage };
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        res.end(`data: ${JSON.stringify(text)}\n\ndata: ${JSON.stringify(last)}\n\ndata: [DONE]\n\n`);
        return;
      }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ id: 'e2e-fake', object: 'chat.completion', created: 0, model: 'e2e-fake',
        choices: [{ index: 0, message: { role: 'assistant', content: 'vault ok' }, finish_reason: 'stop' }], usage }));
    });
  });
  const port = await freePort();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', resolve);
  });
  return {
    base: `http://127.0.0.1:${port}`,
    requests,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
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
  return chromium.launch({ headless: true, args: ['--no-sandbox', `--${RUN_TAG}`] });
}

// `cookie` ({ name, value, url }) is set in the new context before the page
// loads: an owner browser for the owner-only steps (config, shell, steer).
export async function openViewer(browser, url, label, cookie = null) {
  // A tall page: the transcript is a virtual list, so rows scrolled out of
  // view are not in the DOM. The scenarios assert on what a session shows,
  // not on how much fits a laptop screen, so the page is tall enough to hold
  // a scenario's whole transcript.
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 2400 } });
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
  // One more: an invited viewer's page asks for the push key and is
  // refused (403) by design. Only that exact request is allowed.
  const pushProbe = (m) => /status of 403/.test(m.text()) && /\/api\/push\/key(\?|$)/.test(m.location()?.url || '');
  page.on('console', (m) => {
    if (m.type() === 'error' && !expected.test(m.text()) && !pushProbe(m)) v.errors.push(m.text());
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

// The approval type-ahead guard (app/src/chat.rs APPROVAL_KEY_GUARD, 300 ms):
// a tap on an approval option in the first 300 ms of its prompt is ignored.
// Waits on the page clock until the window of the prompt on screen has
// closed, as a user reading the prompt would. The observer stamps each
// prompt when "allow once" first shows and clears the stamp when it goes.
// Without an approval on screen (a picker's own "deny" row) it returns at once.
export async function waitPastPromptGuard(v) {
  await v.page.evaluate(() => {
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
  const open = await v.page.evaluate(() => window.__promptSeen !== null);
  if (!open) return;
  await v.page.waitForFunction(
    () => window.__promptSeen === null || Date.now() - window.__promptSeen >= 300,
    null,
    { timeout: 15000 },
  );
}

const APPROVAL_TAPS = new Set(['allow once', 'allow all', 'deny']);

export async function clickText(v, text) {
  if (APPROVAL_TAPS.has(text)) await waitPastPromptGuard(v);
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
