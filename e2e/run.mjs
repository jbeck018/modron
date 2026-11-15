// Packet E2E: real `modron serve` (mock provider) -> real bridge Worker
// (wrangler dev --local) -> real web app in headless Chromium.
// Run:  node /home/user/modron/e2e/run.mjs      (see e2e/README.md)
import { randomBytes } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  Local, SCRATCH, api, bodyText, clickText, copyApp, killAll, launchBrowser, liveCount,
  openViewer, ownerCookie, refused, sleep, shot, startBridge, startServe, tail, toHttp,
  typeAndSend, waitBody, waitUntil, wsFrames,
} from './lib.mjs';

const hex = () => randomBytes(16).toString('hex');
const STEP_MS = 240000;
const results = [];
const bridges = [];
const serves = [];
const locals = [];
const viewers = [];
const track = (list, x) => { list.push(x); return x; };

// Run a step: body(s) records checks; a throw or failed check is a FAIL.
async function scenario(id, title, body) {
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
  const ok = s.fails.length === 0;
  results.push({ id, title, ok, fails: s.fails });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${id}${ok ? '' : ' -- ' + s.fails.join(' | ')}`);
  if (!ok) dumpEvidence(id, s);
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

function dumpEvidence(id, s) {
  const ev = {
    id,
    fails: s.fails,
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
      consoleErrors: v.errors.slice(-15),
      sockets: v.sockets.map((so) => ({ url: so.url, frames: so.frames.slice(0, 40).map(([d, p]) => [d, p.slice(0, 400)]) })),
    })),
  };
  const path = join(SCRATCH, `evidence-${id.split(' ')[0]}.json`);
  writeFileSync(path, JSON.stringify(ev, null, 2));
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
  await sleep(5000);
  const text = await bodyText(late);
  s.check(text.includes('BIGMARK-0'), `earlier rows missing (page shows ${text.length} chars; BIGMARK-0 absent)`);
  s.check(text.includes('BIGMARK-24') || text.includes('BIGMARK-0'), 'no history row at all');
});

// S7: portal mode ----------------------------------------------------------------
let portal;
try {
  portal = await startBridge('portal', { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST });
  bridges.push(portal);
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

// ---- wrap up ----------------------------------------------------------------------
await browser.close().catch(() => {});
killAll('SIGTERM');
await sleep(2000);
killAll('SIGKILL');
await sleep(500);

const failed = results.filter((r) => !r.ok);
console.log('\n== summary');
for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'} ${r.id} ${r.title}`);
console.log(`${results.length - failed.length}/${results.length} passed; left-over tracked processes: ${liveCount()}`);
writeFileSync(join(SCRATCH, 'results.json'), JSON.stringify(results, null, 2));
console.log(`results: ${join(SCRATCH, 'results.json')}`);
process.exit(failed.length ? 1 : 0);
