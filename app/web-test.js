// Browser tests for the web host (run from app/ after `nsh package --target web -o dist`).
//   (a) demo mode: a transcript row and the status line show as DOM text.
//   (b) live mode: a scripted WebSocket server (below, RFC 6455 by hand) sends
//       welcome, a snapshot and a streamed delta; typing + Enter sends a submit.
// Static files come from `python3 -m http.server`. Uses only Node built-ins
// and the preinstalled Playwright (headless Chromium).
const { chromium } = require('playwright');
const { spawn } = require('child_process');
const crypto = require('crypto');
const http = require('http');
const net = require('net');
const path = require('path');

const DIST = path.join(__dirname, 'dist', 'Modron');
const SESSION = '5b1e8f4a-2c3d-4e5f-8a9b-0c1d2e3f4a5b';
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

function startStatic(port) {
  const child = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1', '--directory', DIST], { stdio: 'ignore' });
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + 10000;
    const poll = () => {
      const req = http.get({ host: '127.0.0.1', port, path: '/' }, (res) => {
        res.resume();
        resolve(child);
      });
      req.on('error', () => {
        if (Date.now() > deadline) return reject(new Error('static server did not start'));
        setTimeout(poll, 100);
      });
    };
    poll();
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
        }
        frames.push(frame);
      }
    });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => {
    resolve({ url: `ws://127.0.0.1:${server.address().port}/`, submitted, received, frames, close: () => server.close() });
  }));
}

async function openPage(browser, url) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 800 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  return { page, errors };
}

async function testDemo(browser, base) {
  const { page, errors } = await openPage(browser, base + '/');
  await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: TIMEOUT });
  const text = await page.evaluate(() => document.body.innerText);
  if (!/claude-opus-5 · 0 tok/.test(text)) throw new Error('status line missing:\n' + text);
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
  await page.waitForFunction(() => document.activeElement && document.activeElement.tagName === 'INPUT', null, { timeout: TIMEOUT });
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
    page.on('pageerror', (e) => errors.push(String(e)));
    await page.goto(base + '/?url=' + encodeURIComponent(ws.url), { waitUntil: 'domcontentloaded' });
    await page.waitForFunction((s) => document.body.innerText.includes(s), STREAM, { timeout: TIMEOUT });
    await page.waitForFunction(() => document.activeElement && document.activeElement.tagName === 'INPUT', null, { timeout: TIMEOUT });
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

(async () => {
  const staticPort = await freePort();
  const staticServer = await startStatic(staticPort);
  const base = `http://127.0.0.1:${staticPort}`;
  const ws = await startWsServer();
  const browser = await chromium.launch({ args: ['--no-sandbox'] });
  let failed = 0;
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
  ];
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
    staticServer.kill();
  }
  process.exit(failed ? 1 : 0);
})();
