// Packet E2E: real `modron serve` (mock provider) -> real bridge Worker
// (wrangler dev --local) -> real web app in headless Chromium.
// Run:  node /home/user/modron/e2e/run.mjs      (see e2e/README.md)
import { createDecipheriv, createECDH, createVerify, generateKeyPairSync, hkdfSync, randomBytes } from 'node:crypto';
import { createServer } from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, writeFileSync, mkdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import {
  freePort, MODRON, REPO, Local, SCRATCH, api, bodyText, clickContains, clickText, copyApp, inviteUrlIn,
  killEverything, taggedCount, launchBrowser, liveCount, newestMtime, openViewer, ownerCookie, ownerCookieFrom, refused, sleep,
  shot, snapshotViewer, startBridge, startFakeOpenAi, startServe, stopProc, tail, toHttp, typeAndSend, waitBody,
  waitPastPromptGuard, waitUntil, wsFrames,
} from './lib.mjs';

const hex = () => randomBytes(16).toString('hex');
const STEP_MS = 240000;
const results = [];
const bridges = [];
const serves = [];
const locals = [];
const viewers = [];
const track = (list, x) => { list.push(x); return x; };

// --only S1,P2,W1 (prefixes allowed: S7 runs S7a to S7d, P runs every P step).
const ONLY = (() => {
  const i = process.argv.indexOf('--only');
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1].split(',').map((x) => x.trim()).filter(Boolean) : null;
})();
const want = (id) => !ONLY || ONLY.some((o) => id === o || id.startsWith(o));
function skipScenario(id, title, reason) {
  if (want(id) || reason !== 'not selected by --only') results.push({ id, title, ok: true, skipped: true, fails: [reason] });
  if (!want(id)) return;
  console.log(`\n== ${id}: ${title}\nSKIP ${id} -- ${reason}`);
}

// Run a step: body(s) records checks; a throw or failed check is a FAIL.
async function scenario(id, title, body) {
  if (!want(id)) return;
  const s = { id, fails: [] };
  s.check = (ok, msg) => { if (!ok) s.fails.push(msg); return ok; };
  console.log(`\n== ${id}: ${title}`);
  // A per-step ceiling so one hung wait cannot stall the whole run.
  const ceiling = sleep(STEP_MS).then(() => { throw new Error(`step exceeded ${STEP_MS / 1000} s`); });
  try {
    await Promise.race([body(s), ceiling]);
  } catch (e) {
    s.fails.push('exception: ' + e.message);
  }
  checkConsole(s);
  const ok = s.fails.length === 0;
  results.push({ id, title, ok, fails: s.fails });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${id}${ok ? '' : ' -- ' + s.fails.join(' | ')}`);
  if (!ok) await dumpEvidence(id, s);
}

// Every viewer's console errors and page errors fail the step that observed
// them. Each distinct message is reported once per step.
const seenErrors = new Set();
function checkConsole(s) {
  for (const v of viewers) {
    for (const e of v.errors) {
      const key = `${v.label}|${e}`;
      if (seenErrors.has(key)) continue;
      seenErrors.add(key);
      s.fails.push(`console error in ${v.label}: ${e.slice(0, 200)}`);
    }
  }
}

// Soft wait: a timeout becomes a check failure instead of ending the step.
async function soft(s, promise, msg) {
  try {
    return await promise;
  } catch (e) {
    s.fails.push(`${msg}: ${e.message}`);
    return null;
  }
}

async function dumpEvidence(id, s) {
  const ev = {
    id,
    fails: s.fails,
    // Each viewer's visible text, its role=button elements and console errors.
    pages: await Promise.all(viewers.map((v) => snapshotViewer(v).catch((e) => ({ label: v.label, error: e.message })))),
    serveLogs: serves.map((x) => ({ name: x.name, tail: tail(x.log, 40) })),
    bridgeLogs: bridges.map((x) => ({ name: x.name, tail: tail(x.log, 40) })),
    localFrames: locals.map((l) => ({
      serve: l.serve.name,
      last: l.frames.slice(-30).map((f) => JSON.stringify(f).slice(0, 300)),
    })),
    viewers: viewers.map((v) => ({
      label: v.label,
      requested: v.requested,
      finalUrl: v.finalUrl,
      consoleErrors: v.errors.slice(-40),
      sockets: v.sockets.map((so) => ({ url: so.url, frames: [...so.frames.filter(([d]) => d === 'out'), ...so.frames.filter(([d]) => d !== 'out').slice(0, 20), ...so.frames.filter(([d]) => d !== 'out').slice(-60)].map(([d, p]) => [d, p.slice(0, 400)]) })),
    })),
  };
  const path = join(SCRATCH, `evidence-${id.split(' ')[0]}.json`);
  writeFileSync(path, JSON.stringify(ev, null, 2));
  for (const v of viewers) await shot(v, `${id}-${v.label}`);
  console.log(`   evidence: ${path}`);
}

// ---- state shared between steps --------------------------------------------
let browser;
let legacy;          // bridge with no portal secrets
let p1, l1, sid1, v1, share1;   // S1-S2, S6a
const OWNER = hex();
const HOST = hex();

// Preflight: copy the bundle, start the legacy bridge and a browser.
copyApp();
legacy = track(bridges, await startBridge('legacy'));
browser = await launchBrowser();
console.log(`bridge up: ${legacy.ws} (scratch ${SCRATCH})`);

// S1 -------------------------------------------------------------------------
await scenario('S1', 'legacy relay: share URL opens /app/, shows connected and history', async (s) => {
  p1 = track(serves, await startServe('p1', { bridgeWs: legacy.ws }));
  l1 = track(locals, new Local(p1));
  await l1.connect();
  l1.setAuto(true); // answer the mock's bash/write approvals locally
  sid1 = await l1.run('e2e history task one');
  await l1.turnDone(sid1);
  share1 = await l1.rc(sid1, legacy.ws);
  s.check(share1.includes('/s/'), `share URL shape: ${share1}`);
  // Product issue candidate: the returned share URL keeps the ws scheme.
  s.check(!/^wss?:/.test(share1), `share URL has a ws scheme, a browser cannot open it: ${share1}`);
  v1 = track(viewers, await openViewer(browser, toHttp(share1), 'viewer1'));
  s.check(v1.finalUrl.includes('/app/?url='), `redirect did not land on /app/?url=: ${v1.finalUrl}`);
  await soft(s, waitBody(v1, (t) => t.includes('connected'), 30000, 'connected'), 'no "connected" status');
  await soft(s, waitBody(v1, (t) => t.includes('e2e history task one') && t.includes('Demo complete'), 30000, 'history rows'),
    'earlier transcript not shown');
  await shot(v1, 's1-viewer1');
});

// S2 -------------------------------------------------------------------------
await scenario('S2', 'browser drives the same session: typed task starts a run for that session', async (s) => {
  const from = l1.frames.length;
  const before = (await l1.sessions()).length;
  await typeAndSend(v1, 'e2e browser task two');
  await soft(s, l1.waitFor((f) => JSON.stringify(f).includes('e2e browser task two'), 30000, 'task echo', from),
    'server never echoed the browser task');
  await soft(s, l1.turnDone(sid1, 60000, from), 'run did not finish');
  const fresh = l1.frames.slice(from);
  s.check(fresh.filter((f) => f.session && f.session !== sid1).length === 0, 'frames for a different session');
  s.check(!fresh.some((f) => f.type === 'error'), `error frame(s): ${JSON.stringify(fresh.filter((f) => f.type === 'error')).slice(0, 300)}`);
  s.check((await l1.sessions()).length === before, 'a new session was created');
  await soft(s, waitBody(v1, (t) => t.includes('e2e browser task two'), 20000, 'browser task row'), 'browser task not shown');
});

// S6a: late viewer, small history (buffered snapshot + live frames) -------------
await scenario('S6a', 'late viewer, small history: fresh viewer sees earlier rows', async (s) => {
  const late = track(viewers, await openViewer(browser, toHttp(share1), 'late-small'));
  await soft(s, waitBody(late, (t) => t.includes('connected'), 30000, 'connected'), 'no connected');
  await soft(s, waitBody(late, (t) => t.includes('e2e history task one'), 20000, 'task one row'),
    'task one (from the tunnel-start snapshot) missing');
  await soft(s, waitBody(late, (t) => t.includes('e2e browser task two'), 20000, 'task two row'),
    'task two (live frames buffered after tunnel start) missing');
});

// S3 -------------------------------------------------------------------------
await scenario('S3', 'approve from the browser: "allow once" resolves the approval and the run finishes', async (s) => {
  const p2 = track(serves, await startServe('p2', { bridgeWs: legacy.ws }));
  const l2 = track(locals, new Local(p2));
  await l2.connect(); // approvals are NOT auto-answered here
  const sid2 = await l2.run('e2e approve task');
  const req = await l2.waitFor((f) => f.type === 'approval_requested' && f.session === sid2, 60000, 'approval_requested');
  const share2 = await l2.rc(sid2, legacy.ws);
  const v3 = track(viewers, await openViewer(browser, toHttp(share2), 'viewer3-approve'));
  await soft(s, waitBody(v3, (t) => t.includes('allow once'), 30000, 'allow once button'), 'approval strip not shown');
  await clickText(v3, 'allow once');
  await soft(s, l2.waitFor((f) => f.type === 'approval_resolved' && f.request_id === req.request_id, 20000, 'approval_resolved'),
    'approval never resolved');
  l2.setAuto(true); // drain the write and bash approvals that follow
  await soft(s, l2.turnDone(sid2, 90000), 'run did not finish after approval');
  await shot(v3, 's3-viewer3');
});

// S4 -------------------------------------------------------------------------
await scenario('S4', 'stop from the browser aborts the active run', async (s) => {
  const p3 = track(serves, await startServe('p3', { bridgeWs: legacy.ws }));
  const l3 = track(locals, new Local(p3));
  await l3.connect();
  const sid3 = await l3.run('e2e stop task');
  await l3.waitFor((f) => f.type === 'approval_requested' && f.session === sid3, 60000, 'approval_requested (run is active)');
  const share3 = await l3.rc(sid3, legacy.ws);
  const v4 = track(viewers, await openViewer(browser, toHttp(share3), 'viewer4-stop'));
  await soft(s, waitBody(v4, (t) => t.includes('stop') && t.includes('allow once'), 30000, 'stop control while running'),
    'stop control not shown while running');
  const from = l3.frames.length;
  await clickText(v4, 'stop');
  await soft(s, l3.waitFor((f) => f.type === 'turn_aborted' && f.session === sid3, 20000, 'turn_aborted', from),
    'run was not aborted');
  s.check(!wsFrames(v4, 'out', 'submit').length, 'stop was sent as a submit');
  await soft(s, waitUntil(async () => !(await bodyText(v4)).includes('allow once'), 15000, 'approval strip cleared'),
    'approval strip still shown after stop');
  await shot(v4, 's4-viewer4');
});

// S5 -------------------------------------------------------------------------
await scenario('S5', 'two viewers: one answers an approval, the other sees it resolve', async (s) => {
  const p5 = track(serves, await startServe('p5', { bridgeWs: legacy.ws }));
  const l5 = track(locals, new Local(p5));
  await l5.connect();
  const sid5 = await l5.run('e2e two viewers task');
  const req5 = await l5.waitFor((f) => f.type === 'approval_requested' && f.session === sid5, 60000, 'approval_requested');
  const share5 = toHttp(await l5.rc(sid5, legacy.ws));
  const va = track(viewers, await openViewer(browser, share5, 'viewerA'));
  const vb = track(viewers, await openViewer(browser, share5, 'viewerB'));
  await soft(s, waitBody(va, (t) => t.includes('allow once'), 30000, 'A approval'), 'A shows no approval');
  await soft(s, waitBody(vb, (t) => t.includes('allow once'), 30000, 'B approval'), 'B shows no approval');
  await clickText(va, 'allow once');
  await soft(s, waitUntil(() => wsFrames(vb, 'in', 'approval_resolved').some(([, p]) => p.includes(req5.request_id)), 20000,
    'B receives approval_resolved'), 'viewer B did not see the approval resolve');
  s.check(wsFrames(va, 'out', 'approve').some(([, p]) => p.includes(req5.request_id)), 'A did not send approve');
  await soft(s, waitBody(va, (t) => t.includes('e2e two viewers task'), 20000, 'A task row'), 'A transcript');
  await soft(s, waitBody(vb, (t) => t.includes('e2e two viewers task'), 20000, 'B task row'), 'B transcript');
  l5.setAuto(true);
  await soft(s, l5.turnDone(sid5, 90000), 'run did not finish');
});

// S6b: late viewer, history over 96 KB ---------------------------------------
await scenario('S6b', 'late viewer, history over 96 KB: earlier rows (expected gap per NOTES.md)', async (s) => {
  const p6 = track(serves, await startServe('p6', { bridgeWs: legacy.ws }));
  const l6 = track(locals, new Local(p6));
  await l6.connect();
  l6.setAuto(true);
  let sid6 = null;
  for (let i = 0; i < 25; i++) {
    const task = `BIGMARK-${i} ` + 'lorem ipsum dolor sit amet '.repeat(220);
    const from = l6.frames.length;
    sid6 = await l6.run(task, sid6);
    await l6.turnDone(sid6, 60000, from);
  }
  const snap = await l6.attach(sid6);
  const bytes = JSON.stringify(snap).length;
  s.check(bytes > 96 * 1024, `setup: snapshot is only ${bytes} bytes`);
  console.log(`   snapshot size before tunnel: ${bytes} bytes`);
  const share6 = toHttp(await l6.rc(sid6, legacy.ws));
  const late = track(viewers, await openViewer(browser, share6, 'late-large'));
  await soft(s, waitBody(late, (t) => t.includes('connected'), 30000, 'connected'), 'no connected');
  await sleep(3000);
  // The transcript is a virtual list: only the rows in view are in the DOM.
  // A web user reaches the older rows with the wheel (the page does not
  // scroll natively; the app maps wheel turns to transcript rows).
  let text = await bodyText(late);
  s.check(text.includes('BIGMARK-24') || text.includes('BIGMARK-23'), 'no newest history row at all');
  for (let i = 0; i < 120 && !text.includes('BIGMARK-0'); i++) {
    await late.page.mouse.move(500, 300);
    await late.page.mouse.wheel(0, -800);
    await sleep(120);
    text = await bodyText(late);
  }
  s.check(text.includes('BIGMARK-0'), `earlier rows not reachable by the wheel (page shows ${text.length} chars; BIGMARK-0 absent)`);
});

// S7: portal mode ----------------------------------------------------------------
let portal;
const needPortal = !ONLY || ONLY.some((o) => 'S7'.startsWith(o) || o.startsWith('S7'));
try {
  if (needPortal) {
    portal = await startBridge('portal', { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST });
    bridges.push(portal);
  }
} catch (e) {
  console.log('FAIL S7 setup: could not start the portal bridge: ' + e.message);
  results.push({ id: 'S7', title: 'portal', ok: false, fails: ['setup: ' + e.message] });
}

if (portal) {
  let cookie = null;
  try {
    cookie = await ownerCookie(portal.base, OWNER);
  } catch (e) {
    console.log('note: POST /auth failed: ' + e.message);
  }

  await scenario('S7a', 'portal: serve WITHOUT MODRON_HOST_TOKEN is refused by the bridge', async (s) => {
    const p7n = track(serves, await startServe('p7-nohost', { bridgeWs: portal.ws }));
    const l7n = track(locals, new Local(p7n));
    await l7n.connect();
    l7n.setAuto(true);
    const sid = await l7n.run('e2e nohost task');
    await l7n.turnDone(sid);
    await l7n.rc(sid, portal.ws);
    await soft(s, waitUntil(() => tail(p7n.log, 200).includes('remote-control tunnel ended'), 15000, 'tunnel ended warning'),
      'serve did not report the tunnel as ended');
    s.check(await refused(`${portal.ws}/agent/${hex()}`), 'bridge accepted an /agent upgrade with no bearer token');
  });

  await scenario('S7b', 'portal: host tunnel connects, session listed after owner sign-in, bare share link refused', async (s) => {
    const p7 = track(serves, await startServe('p7', { bridgeWs: portal.ws, hostToken: HOST }));
    const l7 = track(locals, new Local(p7));
    await l7.connect();
    l7.setAuto(true);
    const sid7 = await l7.run('e2e portal task');
    await l7.turnDone(sid7);
    const share7 = await l7.rc(sid7, portal.ws);
    s.check(cookie !== null, 'POST /auth did not return a cookie');
    if (cookie) {
      const listed = await soft(s, waitUntil(async () => {
        const r = await api(portal.base, cookie, 'GET', '/api/sessions');
        return JSON.stringify(r.json).includes(sid7) ? r : null;
      }, 40000, 'session in GET /api/sessions'), 'session never listed');
      s.check(listed === null || listed.status === 200, 'GET /api/sessions not 200');
    }
    const vkey = share7.split('/s/')[1];
    s.check(vkey && (await refused(`${portal.ws}/ws/${vkey}`)), 'bare /ws/<key> without a cookie opened');
    globalThis.portalSid7 = sid7;
    globalThis.l7 = l7;
  });

  await scenario('S7c', 'portal: viewer invite loads the app, typed message is forbidden', async (s) => {
    const sid7 = globalThis.portalSid7;
    const l7 = globalThis.l7;
    if (!sid7 || !cookie) throw new Error('S7b did not set up a session');
    const inv = await api(portal.base, cookie, 'POST', '/api/invites', { session_id: sid7, role: 'viewer', ttl_hours: 1 });
    s.check(inv.status === 201, `POST /api/invites returned ${inv.status}`);
    const url = new URL(inv.json.url, portal.base);
    const v7 = track(viewers, await openViewer(browser, `${portal.base}${url.pathname}${url.search}`, 'invite-viewer'));
    s.check(v7.finalUrl.includes('/app/?url='), `invite did not redirect to the app: ${v7.finalUrl}`);
    await soft(s, waitBody(v7, (t) => t.includes('connected'), 30000, 'connected'), 'no connected');
    const mark = l7.frames.length;
    await typeAndSend(v7, 'e2e forbidden typed');
    await sleep(4000);
    s.check(!l7.frames.slice(mark).some((f) => JSON.stringify(f).includes('e2e forbidden typed')),
      'a viewer-role submit reached the server');
    s.check(wsFrames(v7, 'in', 'forbidden').length > 0, 'no "forbidden" error came back to the viewer');
  });

  await scenario('S7d', 'portal: approver invite approves but cannot submit', async (s) => {
    const p9 = track(serves, await startServe('p9', { bridgeWs: portal.ws, hostToken: HOST }));
    const l9 = track(locals, new Local(p9));
    await l9.connect(); // approvals are NOT auto-answered
    const sid9 = await l9.run('e2e approver task');
    const req9 = await l9.waitFor((f) => f.type === 'approval_requested' && f.session === sid9, 60000, 'approval_requested');
    const share9 = await l9.rc(sid9, portal.ws);
    const vkey9 = share9.split('/s/')[1];
    await soft(s, waitUntil(async () => {
      const r = await api(portal.base, cookie, 'GET', '/api/sessions');
      return JSON.stringify(r.json).includes(sid9) ? r : null;
    }, 40000, 'approver session listed'), 'approver session never listed');
    const inv = await api(portal.base, cookie, 'POST', '/api/invites', { session_id: sid9, role: 'approver', ttl_hours: 1 });
    s.check(inv.status === 201, `approver invite returned ${inv.status}`);
    const url = new URL(inv.json.url, portal.base);
    s.check(vkey9 && url.pathname.startsWith('/i/'), 'invite url shape');
    const v9 = track(viewers, await openViewer(browser, `${portal.base}${url.pathname}${url.search}`, 'invite-approver'));
    await soft(s, waitBody(v9, (t) => t.includes('allow once'), 30000, 'approver sees approval'), 'approver sees no approval');
    await clickText(v9, 'allow once');
    await soft(s, l9.waitFor((f) => f.type === 'approval_resolved' && f.request_id === req9.request_id, 20000, 'approval_resolved'),
      'approver approve did not resolve');
    const mark = l9.frames.length;
    await typeAndSend(v9, 'e2e approver typed');
    await sleep(4000);
    s.check(!l9.frames.slice(mark).some((f) => JSON.stringify(f).includes('e2e approver typed')),
      'an approver-role submit reached the server');
    s.check(wsFrames(v9, 'in', 'forbidden').length > 0, 'no "forbidden" error for the approver submit');
    l9.setAuto(true);
    await soft(s, l9.turnDone(sid9, 90000), 'approver run did not finish');
  });
}

// ---- P: portal through the UI (own portal bridge, MODRON_TEST_HOOKS=1) -------
// P1 signs in and keeps the owner browser (pOwner) for P2 to P4. Each P step
// needs the one before it (see README).
let pb = null;                 // portal bridge with the test clock
let pOwner = null;             // owner browser context, signed in by P1
let sidA = null, shareA = null, lA = null;   // P1: serve A, history
let sidB = null, shareB = null, lB = null;   // P2: serve B, driven from the browser

async function ensurePortalHooks() {
  if (!pb) pb = track(bridges, await startBridge('portal-hooks', { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, MODRON_TEST_HOOKS: '1' }));
  return pb;
}

await scenario('P1', 'portal sign-in through the UI: wrong token refused, right token lists and opens the session', async (s) => {
  const b = await ensurePortalHooks();
  const sa = track(serves, await startServe('pa', { bridgeWs: b.ws, hostToken: HOST }));
  lA = track(locals, new Local(sa));
  await lA.connect();
  lA.setAuto(true);
  sidA = await lA.newSession('e2e portal A');
  await lA.attach(sidA);
  await lA.run('e2e portal history', sidA);
  await lA.turnDone(sidA);
  shareA = await lA.rc(sidA, b.ws);

  pOwner = track(viewers, await openViewer(browser, b.base + '/', 'owner-ui'));
  s.check(pOwner.finalUrl.includes('/app/'), `GET / did not land on /app/: ${pOwner.finalUrl}`);
  await soft(s, waitBody(pOwner, (t) => t.includes('sign in'), 30000, 'sign-in screen'), 'no sign-in screen');

  const box = pOwner.page.getByPlaceholder('owner token');
  await box.fill('not-the-owner-token');
  await box.press('Enter');
  await soft(s, waitBody(pOwner, (t) => t.includes('wrong token'), 20000, 'wrong token message'), 'no "wrong token" message');
  s.check((await box.inputValue().catch(() => 'unreadable')) === '', 'the token input was not cleared after a wrong token');

  await box.fill(OWNER);
  await pOwner.page.getByRole('button', { name: 'sign in' }).click({ timeout: 15000 });
  const cookie = await soft(s, ownerCookieFrom(pOwner, b.base), 'no owner cookie after sign-in');
  let row = null;
  if (cookie) {
    row = await soft(s, waitUntil(async () => {
      const r = await api(b.base, cookie, 'GET', '/api/sessions');
      return Array.isArray(r.json) ? r.json.find((x) => x.id === sidA) || null : null;
    }, 40000, 'session A in /api/sessions'), 'session A never listed by the portal API');
  }
  if (row) {
    await soft(s, waitBody(pOwner, (t) => t.includes(row.title) && t.includes('online'), 30000, 'session A row online'),
      'the portal list does not show session A as online');
    await soft(s, clickContains(pOwner, row.title).then(() =>
      waitBody(pOwner, (t) => t.includes('e2e portal history') && t.includes('connected'), 30000, 'chat with history')),
    'opening session A did not show its history');
  }
  const leak = await pOwner.page.evaluate(() => ({
    href: location.href, ls: JSON.stringify(localStorage), ss: JSON.stringify(sessionStorage),
  })).catch(() => null);
  s.check(leak !== null, 'could not read location and storage');
  if (leak) {
    s.check(!leak.href.includes(OWNER), 'owner token appears in location.href');
    s.check(!leak.ls.includes(OWNER), 'owner token appears in localStorage');
    s.check(!leak.ss.includes(OWNER), 'owner token appears in sessionStorage');
  }
  s.check(!(await bodyText(pOwner)).includes(OWNER), 'owner token appears in the page text');
  await shot(pOwner, 'p1-owner');
});

await scenario('P2', 'owner drives through the portal: typed task runs, approval answered in the browser', async (s) => {
  if (!pOwner || !pb) throw new Error('P1 did not set up the owner browser');
  const sb = track(serves, await startServe('pb', { bridgeWs: pb.ws, hostToken: HOST }));
  lB = track(locals, new Local(sb));
  await lB.connect(); // approvals are NOT auto-answered here
  sidB = await lB.newSession('e2e portal B');
  await lB.attach(sidB);
  shareB = await lB.rc(sidB, pb.ws);

  await pOwner.page.goto(pb.base + '/', { waitUntil: 'domcontentloaded' });
  await soft(s, waitBody(pOwner, (t) => t.includes('e2e portal B') && t.includes('online'), 40000, 'row B online'),
    'session B not listed online');
  await clickContains(pOwner, 'e2e portal B');
  await soft(s, waitBody(pOwner, (t) => t.includes('connected'), 30000, 'connected'), 'no connected status on session B');

  const from = lB.frames.length;
  await typeAndSend(pOwner, 'e2e portal drive');
  const req = await soft(s, lB.waitFor((f) => f.type === 'approval_requested' && f.session === sidB, 60000,
    'approval_requested', from), 'the browser task never reached an approval');
  await soft(s, waitBody(pOwner, (t) => t.includes('allow once'), 30000, 'approval strip'), 'approval strip not shown in the browser');
  await clickText(pOwner, 'allow once');
  if (req) {
    await soft(s, lB.waitFor((f) => f.type === 'approval_resolved' && f.request_id === req.request_id, 20000, 'approval_resolved'),
      'browser "allow once" did not resolve the approval');
  }
  lB.setAuto(true); // drain the write and bash approvals that follow
  await soft(s, lB.turnDone(sidB, 90000, from), 'run did not finish');
  await soft(s, waitBody(pOwner, (t) => t.includes('e2e portal drive'), 20000, 'task row'), 'task row not shown');
});

await scenario('P3', 'invite from the UI: guest sees the transcript, cannot submit; revoked link refused', async (s) => {
  if (!pOwner || !lB) throw new Error('P2 did not set up session B');
  await clickText(pOwner, 'settings');
  await soft(s, waitBody(pOwner, (t) => t.includes('share'), 20000, 'share section'), 'no share section in Settings');
  await clickText(pOwner, 'viewer');
  const shown = await soft(s, waitBody(pOwner, (t) => /\/i\/[A-Za-z0-9._-]+/.test(t), 20000, 'invite URL'), 'no invite URL shown');
  const url = shown ? inviteUrlIn(await bodyText(pOwner), pb.base) : null;
  s.check(!!url, 'could not read the invite URL from the page');
  if (!url) return;

  const guest = track(viewers, await openViewer(browser, url.href, 'guest-p3')); // fresh context, no cookies
  s.check(guest.finalUrl.includes('/app/?url='), `invite did not redirect into the app: ${guest.finalUrl}`);
  await soft(s, waitBody(guest, (t) => t.includes('connected') && t.includes('e2e portal drive'), 30000, 'guest transcript'),
    'guest does not see the transcript');
  const mark = lB.frames.length;
  await typeAndSend(guest, 'e2e guest typed');
  await soft(s, waitUntil(() => wsFrames(guest, 'in', 'forbidden').length > 0, 15000, 'forbidden'),
    'no "forbidden" reply to the guest submit');
  s.check(!lB.saw('e2e guest typed', mark), 'a viewer submit reached the server');

  await soft(s, waitBody(pOwner, (t) => t.includes('revoke'), 20000, 'revoke control'), 'no revoke control in Settings');
  await clickText(pOwner, 'revoke');
  await sleep(1500);
  const again = track(viewers, await openViewer(browser, url.href, 'guest-after-revoke'));
  await soft(s, waitBody(again, (t) => t.includes('invite is not valid'), 20000, 'refusal text'),
    'a revoked invite was not refused');
  // The refused document is a 410 and the browser logs it as a resource error.
  // That log is the refusal itself, so only this one message is allowed, and
  // only when the main document answered 410.
  s.check(again.status === 410, `revoked invite main document answered ${again.status}, not 410`);
  if (again.status === 410) again.errors = again.errors.filter((e) => !/status of 410/.test(e));
  s.check(!again.finalUrl.includes('/app/?url='), `revoked invite still redirected into the app: ${again.finalUrl}`);
});

await scenario('P4', 'offline marking: a stopped serve is listed offline and cannot be opened', async (s) => {
  if (!pOwner || !shareB || !pb) throw new Error('P2 did not set up session B');
  await stopProc('serve-pb');
  // Test clock: this page's /api/sessions reads are six minutes ahead (x-test-now).
  await pOwner.page.route('**/api/sessions', (route) => route.continue({
    headers: { ...route.request().headers(), 'x-test-now': String(Date.now() + 6 * 60 * 1000) },
  }));
  const socketsBefore = pOwner.sockets.length;
  await pOwner.page.goto(pb.base + '/', { waitUntil: 'domcontentloaded' });
  await soft(s, waitBody(pOwner, (t) => t.includes('e2e portal B') && t.includes('offline'), 40000, 'row B offline'),
    'session B not shown offline after its serve stopped');
  await clickContains(pOwner, 'e2e portal B').catch(() => {});
  await sleep(3000);
  const vkey = shareB.split('/s/')[1];
  s.check(!pOwner.sockets.slice(socketsBefore).some((so) => so.url.includes(vkey)), 'an offline row opened its viewer socket');
  s.check(!(await bodyText(pOwner)).includes('connected'), 'an offline row reached the chat');
  await pOwner.page.unroute('**/api/sessions').catch(() => {});
});

// ---- R1: remote control at start (serve --remote-control, no client) --------
// The serve opens its own tunnel: no client sends remote_control. The session
// shows in the owner's portal list, and the owner's browser opens it and runs
// a task (approvals answered in the browser).
await scenario('R1', 'serve --remote-control opens the tunnel with no client; browser runs a task', async (s) => {
  const rb = track(bridges, await startBridge('remote-flag', { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST }));
  const sr = track(serves, await startServe('r1', { remoteControl: rb.ws, hostToken: HOST }));
  await soft(s, waitUntil(() => existsSync(sr.shareUrlFile), 60000, 'share url file'), 'serve wrote no share url file');
  const share = existsSync(sr.shareUrlFile) ? readFileSync(sr.shareUrlFile, 'utf8').trim() : '';
  s.check(share.startsWith(rb.base + '/s/'), `share url is not the bridge's viewer page: ${share}`);
  s.check(tail(sr.log, 400).includes('remote control open'), 'serve did not log that remote control opened');

  const cookie = await soft(s, ownerCookie(rb.base, OWNER), 'owner sign-in failed');
  if (!cookie) return;
  const row = await soft(s, waitUntil(async () => {
    const r = await api(rb.base, cookie, 'GET', '/api/sessions');
    return Array.isArray(r.json) ? r.json.find((x) => x.online === true) || null : null;
  }, 60000, 'an online session in /api/sessions'), 'the serve never listed its session in the portal');
  if (!row) return;
  // openViewer takes the cookie as an object; `cookie` is "modron_owner=<value>".
  const ownerCk = { name: 'modron_owner', value: cookie.slice(cookie.indexOf('=') + 1), url: rb.base };
  const v = track(viewers, await openViewer(browser, rb.base + '/', 'owner-r1', ownerCk));
  await soft(s, waitBody(v, (t) => t.includes(row.title) && t.includes('online'), 40000, 'session row online'),
    'the portal list does not show the serve session as online');
  await soft(s, clickContains(v, row.title).then(() =>
    waitBody(v, (t) => t.includes('connected'), 30000, 'connected')), 'the browser did not open the session');

  await soft(s, typeAndSend(v, 'e2e flag task').then(() => waitUntil(async () => {
    const t = await bodyText(v);
    if (t.includes('Demo complete.')) return true;
    if (t.includes('allow once')) {
      await waitPastPromptGuard(v);
      await clickText(v, 'allow once').catch(() => {});
    }
    return null;
  }, 120000, 'task finished in the browser')), 'the browser task did not finish');
  await soft(s, waitBody(v, (t) => t.includes('e2e flag task'), 20000, 'task row'), 'task row not shown');
  await shot(v, 'r1-owner');
});

// ---- C1: provider key from the owner's vault (PLAN 10.9) -------------------
// The owner stores an OpenAI-compatible key in the bridge vault. A serve with
// --portal-credentials and NO key in its environment fetches it at start and
// sends it to a fake endpoint, which records the Authorization header. Then
// the key is removed and the serve restarted (a fresh fetch): the next turn
// must not reach the endpoint with any key. Nothing is written to the serve's
// home or logs: no value is found in any file under it.
function filesContaining(dir, needle) {
  const hits = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) hits.push(...filesContaining(p, needle));
    else if (statSync(p).isFile() && readFileSync(p).includes(needle)) hits.push(p);
  }
  return hits;
}

await scenario('C1', 'vault key reaches the provider with no local key; a removed key stops the next turn', async (s) => {
  const VALUE = 'sk-e2e-vault-' + hex();
  const VAULT_KEY = hex();
  const fake = await startFakeOpenAi();
  try {
    const vault = track(bridges, await startBridge('vault', { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, VAULT_KEY }));
    const cookie = await soft(s, ownerCookie(vault.base, OWNER), 'owner sign-in failed');
    if (!cookie) return;
    const put = await fetch(vault.base + '/api/credentials/openai-compat', {
      method: 'PUT', headers: { cookie, origin: vault.base, 'content-type': 'application/json' },
      body: JSON.stringify({ key: VALUE }),
    });
    s.check(put.status === 204, `vault PUT answered ${put.status}`);
    const list = await api(vault.base, cookie, 'GET', '/api/credentials');
    s.check(!JSON.stringify(list.json).includes(VALUE), 'the vault list carries the value');

    const extraEnv = { OPENAI_BASE_URL: fake.base + '/v1' };
    const serveOpts = { remoteControl: vault.ws, hostToken: HOST, provider: 'openai-compat', portalCredentials: true, extraEnv };
    const sv = track(serves, await startServe('c1', serveOpts));
    await soft(s, waitUntil(() => tail(sv.log, 400).includes('portal credentials applied'), 60000, 'applied log'),
      'the serve never applied the vault keys');
    const environ = readFileSync(`/proc/${sv.pid}/environ`, 'utf8').split('\0');
    s.check(!environ.some((kv) => kv.startsWith('OPENAI_API_KEY=')), 'the serve process has OPENAI_API_KEY in its environment');
    s.check(tail(sv.log, 400).includes('OPENAI_API_KEY'), 'the serve log does not name the fetched env var');

    const lx = track(locals, new Local(sv));
    await soft(s, lx.connect(), 'local client did not connect');
    const sid = await soft(s, lx.run('vault probe task'), 'the probe task did not start');
    if (sid) await soft(s, lx.turnDone(sid, 90000), 'the probe turn did not finish');
    const hits = () => fake.requests.filter((r) => r.path.includes('/chat/completions'));
    s.check(hits().length >= 1, 'the fake endpoint got no request with the vault key');
    s.check(hits().some((r) => r.auth === `Bearer ${VALUE}`), 'the request did not carry the vault key as its bearer');
    s.check(!JSON.stringify(lx.frames).includes(VALUE), 'a client frame carries the value');

    // Remove the key in the vault, then restart the serve: a fresh fetch.
    const del = await fetch(vault.base + '/api/credentials/openai-compat', { method: 'DELETE', headers: { cookie, origin: vault.base } });
    s.check(del.status === 204, `vault DELETE answered ${del.status}`);
    lx.close();
    await stopProc('serve-c1');
    const before = hits().length;
    const sv2 = track(serves, await startServe('c1', serveOpts));
    await soft(s, waitUntil(() => readFileSync(sv2.log, 'utf8').includes('portal credentials applied'), 60000, 'second applied log'),
      'the restarted serve never fetched the keys');
    const lx2 = track(locals, new Local(sv2));
    await soft(s, lx2.connect(), 'second local client did not connect');
    const sid2 = await soft(s, lx2.run('vault removed task'), 'the second task did not start');
    if (sid2) await soft(s, lx2.waitFor((f) => (f.type === 'turn_done' || f.type === 'error') && f.session === sid2, 90000, 'end of the second run'),
      'the second run never ended');
    // openai-compat with a base URL and no key is the keyless local-endpoint
    // path (Ollama, vLLM): the next turn goes out with NO authorization header.
    // That is the "carries none" outcome; a key must never reach the endpoint.
    const afterRemoval = hits().slice(before);
    s.check(afterRemoval.every((r) => r.auth === ''), 'the endpoint saw an authorization header after the key was removed');
    s.check(afterRemoval.every((r) => !r.auth.includes(VALUE)), 'the removed key reached the endpoint');
    s.check(!JSON.stringify(lx2.frames).includes(VALUE), 'a second-run frame carries the value');
    lx2.close();

    // Nothing written to disk: no file under either serve home, nor the bridge log, holds the value.
    s.check(filesContaining(sv.home, VALUE).length === 0, 'a file under the serve home holds the value');
    s.check(filesContaining(join(SCRATCH, 'h-c1'), VALUE).length === 0, 'a file under the serve scratch holds the value');
    s.check(!readFileSync(vault.log, 'utf8').includes(VALUE), 'the bridge log holds the value');
    s.check(!readFileSync(sv2.log, 'utf8').includes(VALUE), 'the restarted serve log holds the value');
  } finally {
    await fake.close();
  }
});

// ---- W: workflow steps against a real serve (fresh serve per step) ----------
// Each W step starts its own serve so the mock script (scripted tool turns
// are consumed once per process) begins at its first turn. The steps run as
// the portal owner: a share link is read-only for configuration and shell
// (docs/remote-control.md, dispatch.rs "not allowed on a share link").
let ownerCk = null;
async function sharedSession(name, title, extra = {}) {
  const b = await ensurePortalHooks();
  const sv = track(serves, await startServe(name, { bridgeWs: b.ws, hostToken: HOST, ...extra }));
  const lx = track(locals, new Local(sv));
  await lx.connect();
  const sid = await lx.newSession(title);
  await lx.attach(sid);
  const share = await lx.rc(sid, b.ws);
  if (!ownerCk) ownerCk = await ownerCookie(b.base, OWNER);
  const [ckName, ...ckValue] = ownerCk.split('=');
  const socketUrl = `${b.ws}/ws/${share.split('/s/')[1]}`;
  const vw = track(viewers, await openViewer(browser, `${b.base}/app/?url=${encodeURIComponent(socketUrl)}`, name + '-owner',
    { name: ckName, value: ckValue.join('='), url: b.base }));
  await waitBody(vw, (t) => t.includes('connected'), 30000, 'connected').catch(() => {});
  return { sv, lx, sid, vw };
}

// P5 -------------------------------------------------------------------------
// Web push (cloud/bridge/README.md "Web push"): an approval request from a
// real serve becomes a push to a subscribed endpoint through the local bridge.
// The push service is a fake on 127.0.0.1 (PUSH_TEST_ORIGIN, honoured only
// under MODRON_TEST_HOOKS). The body is decrypted here with node:crypto and
// must read exactly as the serve wrote it: "approval needed: <tool>".
function decryptPush(body, ua, auth) {
  const idlen = body[20];
  const asPub = body.subarray(21, 21 + idlen);
  const ct = body.subarray(21 + idlen);
  const ikm = Buffer.from(hkdfSync('sha256', ua.computeSecret(asPub), auth,
    Buffer.concat([Buffer.from('WebPush: info\0'), ua.getPublicKey(), asPub]), 32));
  const salt = body.subarray(0, 16);
  const cek = Buffer.from(hkdfSync('sha256', ikm, salt, Buffer.from('Content-Encoding: aes128gcm\0'), 16));
  const nonce = Buffer.from(hkdfSync('sha256', ikm, salt, Buffer.from('Content-Encoding: nonce\0'), 12));
  const d = createDecipheriv('aes-128-gcm', cek, nonce);
  d.setAuthTag(ct.subarray(ct.length - 16));
  const padded = Buffer.concat([d.update(ct.subarray(0, ct.length - 16)), d.final()]);
  return padded.subarray(0, padded.length - 1).toString('utf8');
}

await scenario('P5', 'web push: a serve approval request reaches the subscribed endpoint, encrypted, with the exact text', async (s) => {
  const posts = [];
  const fakePush = createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      posts.push({ url: req.url, headers: req.headers, body: Buffer.concat(chunks) });
      res.writeHead(201);
      res.end();
    });
  });
  await new Promise((r) => fakePush.listen(0, '127.0.0.1', r));
  try {
    const pushOrigin = `http://127.0.0.1:${fakePush.address().port}`;
    const vapid = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
    const vj = vapid.publicKey.export({ format: 'jwk' });
    const vpub = Buffer.concat([Buffer.from([4]), Buffer.from(vj.x, 'base64url'), Buffer.from(vj.y, 'base64url')]).toString('base64url');
    const vpriv = vapid.privateKey.export({ format: 'jwk' }).d;
    const owner5 = hex();
    const host5 = hex();
    const pushB = track(bridges, await startBridge('push', {
      OWNER_TOKEN: owner5, HOST_TOKEN: host5, VAPID_PUBLIC_KEY: vpub, VAPID_PRIVATE_KEY: vpriv,
      VAPID_SUBJECT: 'https://modron.example.invalid', PUSH_TEST_ORIGIN: pushOrigin, MODRON_TEST_HOOKS: '1',
    }));
    const cookie5 = await ownerCookie(pushB.base, owner5);
    const key = await api(pushB.base, cookie5, 'GET', '/api/push/key');
    s.check(key.status === 200 && key.json.key === vpub, `key route answered ${key.status}`);
    const ua = createECDH('prime256v1');
    ua.generateKeys();
    const auth = randomBytes(16);
    const sub = await api(pushB.base, cookie5, 'POST', '/api/push/subscribe', {
      endpoint: pushOrigin + '/push/p5',
      keys: { p256dh: ua.getPublicKey().toString('base64url'), auth: auth.toString('base64url') },
    });
    s.check(sub.status === 204, `subscribe answered ${sub.status}`);

    // hostToken: the serve registers its session, so the push carries the title and the portal URL.
    const sp = track(serves, await startServe('p5push', { bridgeWs: pushB.ws, hostToken: host5 }));
    const l5 = track(locals, new Local(sp));
    await l5.connect();
    const sid5 = await l5.newSession('e2e push');
    await l5.rc(sid5, pushB.ws); // the tunnel carries the Notify frames to the bridge
    const from = l5.frames.length;
    await l5.run('e2e push task', sid5);
    const req = await soft(s, l5.waitFor((f) => f.type === 'approval_requested' && f.session === sid5, 60000,
      'approval_requested', from), 'the task never reached an approval');
    const pushed = await waitUntil(() => posts.length > 0, 30000, 'push POST').catch(() => false);
    s.check(Boolean(pushed), 'no push reached the subscribed endpoint through the bridge');
    const post = posts[0];
    if (post) {
      s.check(post.url === '/push/p5', `push went to ${post.url}`);
      s.check(post.headers['content-encoding'] === 'aes128gcm', 'push body is not aes128gcm');
      const auth0 = String(post.headers.authorization || '');
      const m = /^vapid t=([^,]+), k=(\S+)$/.exec(auth0);
      s.check(Boolean(m) && m[2] === vpub, 'VAPID authorization header is missing or names another key');
      if (m) {
        const [h, p, sig] = m[1].split('.');
        const claims = JSON.parse(Buffer.from(p, 'base64url').toString());
        s.check(claims.aud === pushOrigin, `VAPID aud was ${claims.aud}`);
        s.check(createVerify('SHA256').update(h + '.' + p).verify({ key: vapid.publicKey, dsaEncoding: 'ieee-p1363' }, Buffer.from(sig, 'base64url')),
          'VAPID JWT does not verify under the public key');
      }
      const payload = JSON.parse(decryptPush(post.body, ua, auth));
      const expected = 'approval needed: ' + (req?.call?.name ?? '?');
      s.check(payload.body === expected, `push body was "${payload.body}", expected "${expected}"`);
      s.check(payload.title === 'e2e push', `push title was "${payload.title}"`);
      s.check(String(payload.data?.url || '').startsWith(pushB.base + '/app/?url='), `push URL was ${payload.data?.url}`);
    }
    if (req) l5.approve(req.request_id);
    l5.setAuto(true);
  } finally {
    fakePush.close();
  }
});

await scenario('W1', 'steer and queue from the browser while an approval waits', async (s) => {
  const { lx, sid, vw } = await sharedSession('w1', 'e2e w1');
  const from = lx.frames.length;
  await typeAndSend(vw, 'e2e steer task');
  const req = await soft(s, lx.waitFor((f) => f.type === 'approval_requested' && f.session === sid, 60000,
    'approval_requested', from), 'no approval for the first run');
  if (!req) return;
  await soft(s, waitBody(vw, (t) => t.includes('allow once'), 20000, 'approval strip'), 'approval strip not shown');
  await typeAndSend(vw, 'e2e steer text'); // Enter while running = steer
  await soft(s, lx.waitFor((f) => f.type === 'steered' && String(f.text).includes('e2e steer text'), 20000, 'steered', from),
    'no steered frame for the steer');
  const box = vw.page.getByPlaceholder('Message').first();
  await box.click({ timeout: 15000 });
  await box.fill('e2e queued text');
  await box.press('Alt+Enter'); // queue
  await soft(s, lx.waitFor((f) => f.type === 'queue_changed' && JSON.stringify(f.queued || []).includes('e2e queued text'), 20000,
    'queue_changed', from), 'no queue_changed for the queued message');
  await soft(s, waitBody(vw, (t) => t.includes('e2e queued text'), 20000, 'queue chip'), 'queue chip not shown');
  await clickText(vw, 'allow all'); // covers the rest of this run (write, bash)
  await soft(s, waitUntil(() => lx.frames.slice(from).filter((f) => f.type === 'turn_done' && f.session === sid).length >= 2,
    90000, 'second turn_done'), 'the queued message did not start a run of its own');
  await soft(s, waitBody(vw, (t) => t.includes('e2e steer text') && !t.includes('[1] e2e queued text'), 20000, 'steer echo'),
    'steered text not shown as a task echo, or the queue chip stayed');
});

await scenario('W2', 'plan phase: /plan refuses gated tools in the engine, /build returns to build', async (s) => {
  const { lx, sid, vw } = await sharedSession('w2', 'e2e w2');
  lx.setAuto(true); // none expected in plan phase; auto keeps the run moving if one comes
  const from = lx.frames.length;
  await typeAndSend(vw, '/plan');
  await soft(s, lx.waitFor((f) => f.type === 'config_changed' && JSON.stringify(f).includes('plan'), 15000, 'config plan', from),
    'no config echo for /plan');
  await soft(s, waitBody(vw, (t) => /\bplan\b/.test(t), 15000, 'plan in status'), 'status line does not show plan');
  const mark = lx.frames.length;
  await typeAndSend(vw, 'e2e plan task');
  await soft(s, lx.turnDone(sid, 90000, mark), 'plan run did not finish');
  s.check(lx.saw('plan phase: read-only', mark), 'no "plan phase: read-only" refusal in the server frames');
  s.check(!lx.frames.slice(mark).some((f) => f.type === 'approval_requested'), 'a gated tool asked for approval in plan phase');
  await vw.page.keyboard.press('Control+t').catch(() => {}); // tool detail panel, for the evidence
  await sleep(500);
  await vw.page.keyboard.press('Control+t').catch(() => {});
  const mark2 = lx.frames.length;
  await typeAndSend(vw, '/build');
  await soft(s, lx.waitFor((f) => f.type === 'config_changed' && JSON.stringify(f).includes('build'), 15000, 'config build', mark2),
    'no config echo for /build');
});

// Settings rows open a picker (app/src/nav.rs Screen::Picker): press the row,
// then press the value in the list.
await scenario('W3', 'settings round trip: effort and approval mode picked in Settings echo into the status line', async (s) => {
  const { lx, vw } = await sharedSession('w3', 'e2e w3');
  await clickText(vw, 'settings');
  await soft(s, waitBody(vw, (t) => t.includes('effort'), 20000, 'settings rows'), 'settings screen did not open');
  const mark = lx.frames.length;
  await clickContains(vw, 'effort');
  await soft(s, waitBody(vw, (t) => /(^|\n)\s*\S?\s*low\s*(\n|$)/.test(t), 20000, 'effort picker'), 'the effort picker did not open');
  await clickText(vw, 'low');
  await soft(s, lx.waitFor((f) => f.type === 'config_changed' && JSON.stringify(f).includes('low'), 15000, 'effort echo', mark),
    'server did not echo effort low');
  const second = lx.frames.length;
  await clickContains(vw, 'approval mode');
  await soft(s, waitBody(vw, (t) => /(^|\n)\s*\S?\s*yolo\s*(\n|$)/.test(t), 20000, 'mode picker'), 'the mode picker did not open');
  await clickText(vw, 'yolo');
  await soft(s, lx.waitFor((f) => f.type === 'config_changed', 15000, 'mode echo', second), 'server did not echo the mode change');
  await vw.page.keyboard.press('Escape');
  await soft(s, waitBody(vw, (t) => t.includes('effort low') && t.includes('yolo'), 20000, 'status line'),
    'status line does not show "effort low" and "yolo" after the echo');
});

// PLAN 10.2a: the owner's own `!command` is not approval-prompted (the
// approval mode restricts the model, not the user), `!cmd` joins the
// transcript, and `!!cmd` is private. Deny mode therefore does not refuse it.
await scenario('W4', 'shell passthrough: runs without an approval, output shown; !! is private; deny mode does not block the owner', async (s) => {
  const { lx, sid, vw } = await sharedSession('w4', 'e2e w4');
  lx.setAuto(false);
  const from = lx.frames.length;
  await typeAndSend(vw, '!echo e2e-shell-ok');
  await soft(s, lx.turnDone(sid, 60000, from), 'shell turn did not finish');
  s.check(!lx.frames.slice(from).some((f) => f.type === 'approval_requested'), 'the owner was asked to approve their own shell command');
  await soft(s, waitUntil(async () => {
    const t = await bodyText(vw);
    return /(^|\n)\s*e2e-shell-ok\s*(\n|$)/.test(t);
  }, 20000, 'shell output'), 'shell output not shown in the transcript');

  const priv = lx.frames.length;
  await typeAndSend(vw, '!!echo e2e-shell-private');
  await soft(s, lx.turnDone(sid, 60000, priv), 'private shell turn did not finish');
  s.check(wsFrames(vw, 'out', '"private":true').length > 0, 'the browser did not send a private shell frame for !!');
  await soft(s, waitBody(vw, (t) => t.includes('e2e-shell-private'), 20000, 'private output'), 'private shell output not shown to the user');

  await typeAndSend(vw, '/mode deny');
  await soft(s, lx.waitFor((f) => f.type === 'config_changed', 15000, 'mode deny echo', priv), 'no config echo for /mode deny');
  const mark = lx.frames.length;
  await typeAndSend(vw, '!echo e2e-shell-in-deny-mode');
  await soft(s, lx.turnDone(sid, 60000, mark), 'shell turn in deny mode did not finish');
  s.check(!lx.frames.slice(mark).some((f) => f.type === 'approval_requested'), 'deny mode asked for approval');
  await soft(s, waitBody(vw, (t) => t.includes('e2e-shell-in-deny-mode'), 20000, 'output in deny mode'), 'the owner command did not run in deny mode');
});

// A remote-control tunnel is pinned to one session: the serve refuses list,
// create and delete on it for every role (dispatch.rs relay_tier, "Refused for
// every role"). So the sessions screen opens, shows that it has no list, and
// shows the refusal. Switching sessions is checked on the local terminal app
// (T1) and in the portal list (P1, P2), which do list sessions.
await scenario('W5', 'sessions screen over the tunnel: Ctrl+O opens it, the server refusal is shown', async (s) => {
  const { lx, sid, vw } = await sharedSession('w5', 'e2e w5 first');
  lx.setAuto(true);
  await lx.run('e2e w5 first history', sid);
  await lx.turnDone(sid);
  await soft(s, waitBody(vw, (t) => t.includes('e2e w5 first history'), 30000, 'first history'), 'first history not shown');

  await vw.page.keyboard.press('Control+o');
  await soft(s, waitBody(vw, (t) => t.includes('no sessions yet'), 20000, 'sessions screen'),
    'Ctrl+O did not show the sessions screen');
  await soft(s, waitBody(vw, (t) => t.includes('not available through a remote-control tunnel'), 20000, 'refusal'),
    'the tunnel refusal for list_sessions is not shown on the sessions screen');
});

await scenario('W6', 'compact: /compact on a session with history shows a before/after or nothing-to-compact note', async (s) => {
  const { lx, sid, vw } = await sharedSession('w6', 'e2e w6');
  lx.setAuto(true);
  await lx.run('e2e w6 history', sid);
  await lx.turnDone(sid);
  await soft(s, waitBody(vw, (t) => t.includes('e2e w6 history'), 30000, 'history'), 'history not shown');
  // The compact reply is correlated to the browser's request, so the page
  // (not the local client, which never sees it) is where the note lands.
  await typeAndSend(vw, '/compact');
  const shown = await soft(s, waitBody(vw, (t) => /nothing to compact|compacted: \d+ -> \d+ tokens/.test(t), 30000, 'compact note shown'),
    'no compact note shown in the browser (the server reply did not arrive)');
  if (shown) {
    s.check(wsFrames(vw, 'out', '"type":"compact"').length > 0, 'the browser socket never sent a compact frame');
  }
});

// W7: the agent picker sets a persona and its first row, "no persona", clears
// it. The serve's own config.toml defines the one persona ([agents.reviewer]).
await scenario('W7', 'agent picker: a persona is set from the picker and "no persona" clears it', async (s) => {
  const { lx, sid, vw } = await sharedSession('w7', 'e2e w7', {
    config: '[agents.reviewer]\nsystem = "E2E review persona: check the diff."\n',
  });
  await typeAndSend(vw, '/agent');
  await soft(s, waitBody(vw, (t) => t.includes('no persona') && t.includes('reviewer - E2E review'), 20000, 'agent picker rows'),
    'the agent picker did not list "no persona" and the configured persona');
  const setMark = lx.frames.length;
  await clickContains(vw, 'reviewer - E2E review');
  await soft(s, lx.waitFor((f) => JSON.stringify(f).includes('"persona":"reviewer"'), 15000, 'persona echo', setMark),
    'server did not echo persona "reviewer"');
  const withPersona = await soft(s, lx.attach(sid), 'no snapshot after the persona switch');
  s.check(withPersona?.live?.persona === 'reviewer',
    `session persona after the pick is ${JSON.stringify(withPersona?.live?.persona)}, not "reviewer"`);

  await typeAndSend(vw, '/agent');
  await soft(s, waitBody(vw, (t) => t.includes('no persona'), 20000, 'agent picker reopened'), 'the agent picker did not reopen');
  const clearMark = lx.frames.length;
  await clickContains(vw, 'no persona');
  await soft(s, lx.waitFor((f) => JSON.stringify(f).includes('"persona":""'), 15000, 'persona clear echo', clearMark),
    'server did not echo the empty persona name');
  const cleared = await soft(s, lx.attach(sid), 'no snapshot after the clear');
  s.check(cleared?.live && cleared.live.persona === undefined,
    `session persona after the clear is ${JSON.stringify(cleared?.live?.persona)}, not absent`);

  await typeAndSend(vw, '/agent');
  await soft(s, waitBody(vw, (t) => /●\s*no persona/.test(t), 20000, 'no persona marked'),
    'the agent picker does not mark "no persona" as current after the clear');
  const marked = await bodyText(vw);
  s.check(!/●\s*reviewer/.test(marked), 'the agent picker still marks "reviewer" as current');
});

// ---- T1: terminal app under a pty (only with a fresh binary) -----------------
const TUI_BIN = join(REPO, 'app/target/debug/modron-app-tui');
const tuiFresh = existsSync(TUI_BIN) && newestMtime(join(REPO, 'app/src')) < statSync(TUI_BIN).mtimeMs;
if (!want('T1')) {
  // not selected: nothing to report
} else if (!tuiFresh) {
  skipScenario('T1', 'terminal app under a pty',
    existsSync(TUI_BIN) ? 'modron-app-tui is older than app/src (not built here; build is not allowed)' : 'modron-app-tui not built');
} else {
  await scenario('T1', 'terminal app under a pty: starts the server, approval, Esc denies, Ctrl+C twice exits', async (s) => {
    const port = await freePort();
    const home = join(SCRATCH, 'h-tui');
    const work = join(home, 'w');
    mkdirSync(work, { recursive: true });
    const cfgPath = join(SCRATCH, 'tui-pty.cfg.json');
    const out = join(SCRATCH, 'tui-pty.json');
    writeFileSync(cfgPath, JSON.stringify({ bin: TUI_BIN, serve_bin: MODRON, url: `ws://127.0.0.1:${port}`, home, cwd: work, out }));
    const run = spawnSync('python3', ['-I', join(REPO, 'e2e/tui_pty.py'), cfgPath], { timeout: 170000, encoding: 'utf8' });
    if (!existsSync(out)) throw new Error(`pty driver produced no output: ${run.stderr || run.error}`);
    const r = JSON.parse(readFileSync(out, 'utf8'));
    s.check(r.connected, 'the app never showed "connected"');
    s.check(r.approval, 'the typed task never reached an approval prompt');
    s.check(r.deny_seen, 'no deny/refusal text after Esc');
    s.check(r.exit_code === 0, `the app did not exit 0 after Ctrl+C twice (exit ${r.exit_code}, still running: ${r.still_running})`);
    s.check(r.resume_line, 'the resume line (--session) was not printed');
    s.check(r.serve_left.length === 0, `server process left behind: ${r.serve_left.join(' | ')}`);
    s.check(!r.still_running, 'the app was still running after Ctrl+C twice');
    console.log(`   tui tail: ${r.tail.slice(-400).replace(/\s+/g, ' ')}`);
  });
}

// ---- T2-T4: the same pty driver in other modes (e2e/tui_pty.py) -------------
// Each mode runs one app under a pty. The driver writes its result JSON to
// `out` and progress markers to `marks`; T4 waits on the markers and asks the
// server itself through the suite's local client.
function startPty(mode, port) {
  const home = join(SCRATCH, `h-${mode}`);
  const work = join(home, 'w');
  const marks = join(SCRATCH, `marks-${mode}`);
  mkdirSync(work, { recursive: true });
  mkdirSync(marks, { recursive: true });
  const cfgPath = join(SCRATCH, `tui-pty-${mode}.cfg.json`);
  const out = join(SCRATCH, `tui-pty-${mode}.json`);
  writeFileSync(cfgPath, JSON.stringify({ mode, marks, bin: TUI_BIN, serve_bin: MODRON, url: `ws://127.0.0.1:${port}`, home, cwd: work, out }));
  const child = spawn('python3', ['-I', join(REPO, 'e2e/tui_pty.py'), cfgPath], { stdio: ['ignore', 'pipe', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (d) => { stderr += d; });
  const exited = new Promise((res) => child.on('exit', () => res()));
  return { child, out, marks, home, stderr: () => stderr, exited };
}

async function ptyResult(p, ms = 170000) {
  const timer = sleep(ms).then(() => 'timeout');
  if ((await Promise.race([p.exited.then(() => 'exit'), timer])) === 'timeout') p.child.kill('SIGKILL');
  if (!existsSync(p.out)) throw new Error(`pty driver produced no output: ${p.stderr()}`);
  return JSON.parse(readFileSync(p.out, 'utf8'));
}

// The task's text in the serve's own session store (<home>/.modron/sessions).
function storedWithin(p, needle, ms = 30000) {
  const store = join(p.home, '.modron', 'sessions');
  const stored = () => existsSync(store) && readdirSync(store)
    .filter((f) => f.endsWith('.jsonl'))
    .some((f) => readFileSync(join(store, f), 'utf8').includes(needle));
  return (async () => {
    const end = Date.now() + ms;
    while (!stored()) {
      if (Date.now() > end) return false;
      await sleep(300);
    }
    return true;
  })();
}

// Waits for the driver's approval marker; false when the app ends first.
async function awaitMark(p, name) {
  const ok = await waitFile(join(p.marks, name));
  if (!ok) await ptyResult(p).catch(() => null);
  return ok;
}

async function waitFile(path, ms = 120000) {
  const end = Date.now() + ms;
  while (!existsSync(path)) {
    if (Date.now() > end) return false;
    await sleep(200);
  }
  return true;
}

function ptyChecks(s, r) {
  s.check(r.connected, 'the app never showed "connected"');
  s.check(r.exit_code === 0, `the app did not exit 0 after Ctrl+C twice (exit ${r.exit_code}, still running: ${r.still_running})`);
  s.check(r.resume_line, 'the resume line (--session) was not printed');
  s.check(r.serve_left.length === 0, `server process left behind: ${r.serve_left.join(' | ')}`);
  s.check(!r.still_running, 'the app was still running after Ctrl+C twice');
}

if (!want('T2') && !want('T3') && !want('T4')) {
  // not selected
} else if (!tuiFresh) {
  for (const id of ['T2', 'T3', 'T4']) {
    skipScenario(id, 'terminal app under a pty', existsSync(TUI_BIN) ? 'modron-app-tui is older than app/src' : 'modron-app-tui not built');
  }
} else {
  await scenario('T2', 'terminal app: Esc inside the approval window is ignored; past it, Esc denies', async (s) => {
    const port = await freePort();
    const r = await ptyResult(startPty('t2', port));
    s.check(r.approval, 'the typed task never reached an approval prompt');
    s.check(r.early_ignored, `an Esc inside the 300 ms window was not ignored (tail: ${String(r.early_tail).slice(-200).replace(/\s+/g, ' ')})`);
    s.check(r.prompt_cleared, 'the Esc past the window did not answer the prompt (no change of prompt)');
    s.check(r.deny_seen, 'the next tool prompt did not follow the denial');
    ptyChecks(s, r);
  });

  await scenario('T3', 'terminal app: one Ctrl+C interrupts the running turn and the app stays open; the second quits', async (s) => {
    const port = await freePort();
    const r = await ptyResult(startPty('t3', port));
    s.check(r.approval, 'the typed task never reached an approval prompt (the running turn)');
    s.check(r.interrupted, 'one Ctrl+C did not end the waiting turn');
    s.check(r.alive_after_first, 'the app exited on the first Ctrl+C');
    ptyChecks(s, r);
  });

  await scenario('T4', 'terminal app: Ctrl+J and Shift+Enter make a three-line draft submitted as one task', async (s) => {
    const port = await freePort();
    const p = startPty('t4', port);
    const r0 = await awaitMark(p, 'm1');
    if (!r0) throw new Error('no approval for the Ctrl+J task');
    // The serve's side: it writes each session to <home>/.modron/sessions as
    // JSON lines, and a task's newline is stored as the two characters \n.
    s.check(await storedWithin(p, 'line one\\nline two\\nline three'), 'the server did not store the Ctrl+J and Shift+Enter task as one three-line task');
    const r = await ptyResult(p);
    s.check(r.typed1 && r.typed2 && r.typed3, 'the three lines were not typed into the composer');
    ptyChecks(s, r);
  });

  await scenario('T4b', 'terminal app: a trailing backslash continues the line; Enter submits both lines as one task', async (s) => {
    const port = await freePort();
    const p = startPty('t4b', port);
    const r0 = await awaitMark(p, 'm1');
    if (!r0) throw new Error('no approval for the backslash task');
    s.check(await storedWithin(p, 'alpha\\nomega'), 'the server did not store the backslash task as one two-line task');
    const r = await ptyResult(p);
    s.check(r.typed1 && r.typed2, 'the backslash continuation was not typed into the composer');
    ptyChecks(s, r);
  });
}

// H1 -------------------------------------------------------------------------
// Handoff between two hosts, git carrier (docs/HANDOFF.md). Host A runs a
// task whose mock script writes demo-output.txt, exports the session to a
// shared bare remote, and keeps serving. Host B clones the remote, imports
// the handoff, serves, and resumes the same session: transcript and file
// change present. Host A then refuses a new run on the handed-off session.
function cli(home, work, args) {
  const r = spawnSync(MODRON, [...args, '--working-dir', work], {
    env: { ...process.env, HOME: home }, encoding: 'utf8', timeout: 120000,
  });
  return { status: r.status, out: `${r.stdout || ''}${r.stderr || ''}` };
}
function git(cwd, args) {
  const r = spawnSync('git', ['-C', cwd, ...args], {
    env: {
      ...process.env, GIT_AUTHOR_NAME: 'e2e', GIT_AUTHOR_EMAIL: 'e2e@example.invalid',
      GIT_COMMITTER_NAME: 'e2e', GIT_COMMITTER_EMAIL: 'e2e@example.invalid',
    },
    encoding: 'utf8',
  });
  return { status: r.status, out: `${r.stdout || ''}${r.stderr || ''}` };
}

await scenario('H1', 'handoff: host A exports a session with a file change; host B imports, resumes it; A refuses runs', async (s) => {
  // Host A's working tree is a git checkout with a shared bare remote. startServe
  // uses SCRATCH/h-<name>/w as its working dir, so the checkout is made there first.
  const remote = join(SCRATCH, 'handoff-remote.git');
  const homeA = join(SCRATCH, 'h-h1a');
  const workA = join(homeA, 'w');
  const homeB = join(SCRATCH, 'h-h1b');
  const workB = join(homeB, 'w');
  mkdirSync(workA, { recursive: true });
  for (const r of [
    git(SCRATCH, ['init', '-q', '--bare', '-b', 'main', remote]),
    git(workA, ['init', '-q', '-b', 'main']),
  ]) if (r.status !== 0) throw new Error('git setup failed: ' + r.out);
  writeFileSync(join(workA, 'README.md'), 'handoff e2e\n');
  for (const args of [
    ['add', '-A'], ['commit', '-q', '-m', 'base'], ['remote', 'add', 'origin', remote], ['push', '-q', 'origin', 'main'],
  ]) {
    const r = git(workA, args);
    if (r.status !== 0) throw new Error(`git ${args[0]} failed: ${r.out}`);
  }

  const svA = track(serves, await startServe('h1a'));
  const la = track(locals, await new Local(svA).connect());
  la.setAuto(true); // the mock's write and bash approvals
  const sid = await la.run('e2e handoff task');
  await la.turnDone(sid, 120000);
  await soft(s, waitUntil(() => existsSync(join(workA, 'demo-output.txt')), 30000, 'demo-output.txt'),
    'the mock write did not land in host A');

  const exp = cli(homeA, workA, ['handoff', 'export', '--session', sid, '--remote', 'origin']);
  s.check(exp.status === 0, 'export on host A failed: ' + exp.out);
  const ref = /modron handoff import (\S+)/.exec(exp.out)?.[1];
  s.check(Boolean(ref), 'export printed no import command: ' + exp.out);
  if (!ref) return;

  // Host A keeps serving; its engine must read the HandedOff line before the next run.
  const from = la.frames.length;
  await la.run('e2e after handoff', sid);
  const refused = await soft(s, la.waitFor((f) => JSON.stringify(f).includes('was handed off to another host'), 30000,
    'handoff refusal', from), 'host A did not refuse a run on the handed-off session');
  if (refused) s.check(JSON.stringify(refused).includes(ref), 'the refusal does not name the carrier ref');

  // Host B: clone the remote (clean tree), import, serve, resume the same session.
  mkdirSync(homeB, { recursive: true });
  const clone = spawnSync('git', ['clone', '-q', remote, workB], { encoding: 'utf8' });
  if (clone.status !== 0) throw new Error('git clone for host B failed: ' + clone.stderr);
  const imp = cli(homeB, workB, ['handoff', 'import', ref, '--remote', 'origin']);
  s.check(imp.status === 0, 'import on host B failed: ' + imp.out);
  if (imp.status !== 0) return;
  s.check(existsSync(join(workB, 'demo-output.txt')), 'the file change did not reach host B');

  const svB = track(serves, await startServe('h1b'));
  const lb = track(locals, await new Local(svB).connect());
  lb.setAuto(true);
  const snap = await lb.attach(sid);
  s.check(JSON.stringify(snap).includes('e2e handoff task'), 'the transcript did not come across the handoff');
  const fromB = lb.frames.length;
  await lb.run('e2e resumed on host b', sid);
  await soft(s, lb.turnDone(sid, 120000), 'host B did not finish a turn on the imported session');
  s.check(lb.frames.slice(fromB).some((f) => JSON.stringify(f).includes('e2e resumed on host b')),
    'host B did not take the resumed task on the same session');
});

// ---- wrap up ----------------------------------------------------------------------
// Always reached: a normal end, or a step's error that escaped its wrapper.
try {
  await browser?.close().catch(() => {});
} finally {
  killEverything('SIGTERM');
  await sleep(2000);
  killEverything('SIGKILL');
  await sleep(500);
}

const failed = results.filter((r) => !r.ok && !r.skipped);
console.log('\n== summary');
for (const r of results) console.log(`${r.skipped ? 'SKIP' : r.ok ? 'PASS' : 'FAIL'} ${r.id} ${r.title}${r.skipped ? ' -- ' + r.fails[0] : ''}`);
const skipped = results.filter((r) => r.skipped).length;
console.log(`${results.length - failed.length - skipped}/${results.length} passed, ${failed.length} failed, ${skipped} skipped; left-over tracked processes: ${liveCount() + taggedCount()}`);
writeFileSync(join(SCRATCH, 'results.json'), JSON.stringify(results, null, 2));
console.log(`results: ${join(SCRATCH, 'results.json')}`);
// E2E_NO_SKIP=1 (CI): a skipped step (for example a stale terminal binary) is
// a failure too, so a green run means every scenario actually ran.
const noSkipFailure = process.env.E2E_NO_SKIP === '1' && skipped > 0;
if (noSkipFailure) console.log(`E2E_NO_SKIP=1: ${skipped} skipped step(s) count as failures`);
process.exit(failed.length || noSkipFailure ? 1 : 0);
