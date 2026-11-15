// Portal checks against real local Workers (`wrangler dev --local`, miniflare
// on this machine; no Cloudflare account is touched). Three instances:
//   A: OWNER_TOKEN + HOST_TOKEN set, MODRON_TEST_HOOKS=1 (expiry and clock hooks).
//   B: no portal secrets (the legacy share-link deployment).
//   C: OWNER_TOKEN + HOST_TOKEN set, no test hooks (the hooks must be inert).
// Secrets are random per run and passed as `--var`; each instance gets its
// own persist directory under the gitignored .wrangler/, deleted afterwards.
// Run from cloud/bridge:  node test/portal.mjs
import { spawn } from "node:child_process";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const bridgeDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const appDir = join(bridgeDir, "public", "app");
const stateRoot = join(bridgeDir, ".wrangler", "portal-test-" + randomBytes(6).toString("hex"));
const failures = [];
const check = (name, ok, detail = "") => {
  console.log((ok ? "PASS " : "FAIL ") + name + (ok || !detail ? "" : " -- " + detail));
  if (!ok) failures.push(name);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Placeholder bundle, only when the real one (sync-app.sh) is absent.
let madePlaceholder = false;
if (!existsSync(join(appDir, "index.html"))) {
  mkdirSync(appDir, { recursive: true });
  writeFileSync(join(appDir, "index.html"), "<!doctype html><title>placeholder</title><p>modron-placeholder-app</p>\n");
  writeFileSync(join(appDir, "app.js"), "// placeholder\n");
  writeFileSync(join(appDir, "nsh.abcdef0123.wasm"), Buffer.from([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]));
  madePlaceholder = true;
}

const freePort = () => new Promise((resolve, reject) => {
  const srv = createServer();
  srv.unref();
  srv.on("error", reject);
  srv.listen(0, "127.0.0.1", () => {
    const { port } = srv.address();
    srv.close(() => resolve(port));
  });
});

// Starts `wrangler dev --local` with the given vars and waits until it serves.
async function startDev(label, vars) {
  const port = await freePort();
  const persist = join(stateRoot, label);
  const args = [
    "wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(port),
    "--show-interactive-dev-session=false", "--persist-to", persist,
  ];
  for (const [name, value] of Object.entries(vars)) args.push("--var", `${name}:${value}`);
  const out = [];
  const child = spawn("npx", args, {
    cwd: bridgeDir,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" },
  });
  child.stdout.on("data", (d) => out.push(String(d)));
  child.stderr.on("data", (d) => out.push(String(d)));
  const base = `http://127.0.0.1:${port}`;
  const end = Date.now() + 90_000;
  let ready = false;
  while (Date.now() < end && !ready) {
    try {
      // Any HTTP answer means the Worker serves (with OWNER_TOKEN set, / is a 302).
      await fetch(base + "/", { redirect: "manual" });
      ready = true;
    } catch { /* not listening yet */ }
    if (!ready) await sleep(500);
  }
  if (!ready) {
    child.kill("SIGTERM");
    throw new Error(`${label}: wrangler dev did not become ready. Output:\n${out.join("")}`);
  }
  console.log(`${label}: wrangler dev up on ${base}`);
  return { base, port, stop: () => child.kill("SIGTERM") };
}

// WebSocket with optional headers (the Node WebSocket accepts them).
function connect(url, headers) {
  const ws = new WebSocket(url, headers ? { headers } : undefined);
  const got = [];
  ws.onmessage = (e) => got.push(String(e.data));
  const opened = new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), 8000);
    ws.onopen = () => { clearTimeout(timer); resolve(true); };
    ws.onerror = () => { clearTimeout(timer); resolve(false); };
    ws.onclose = () => { clearTimeout(timer); resolve(false); };
  });
  return { ws, got, opened };
}

async function waitUntil(pred, ms = 6000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (pred()) return true;
    await sleep(50);
  }
  return pred();
}

const json = (body, extra = {}) => ({
  headers: { "content-type": "application/json", ...(extra.headers || {}) },
  method: extra.method || "POST",
  body: JSON.stringify(body),
});

// Shared secrets and keys for this run.
const OWNER = randomBytes(24).toString("hex");
const HOST = randomBytes(24).toString("hex");
const agentKey = randomBytes(16).toString("hex");
const viewerKey = createHash("sha256").update("modron-viewer:" + agentKey).digest("hex").slice(0, 32);
const sid = randomUUID();
const ipBad = "198.51.100.10"; // rate-limit bucket for failing sign-ins
const ipGood = "198.51.100.20"; // bucket for the good sign-in
const ipHooksOff = "198.51.100.30"; // bucket for instance C
const DAY = 24 * 3600 * 1000;
const MIN = 60 * 1000;
const zeros = "0".repeat(64);

mkdirSync(stateRoot, { recursive: true });
const A = await startDev("portal", { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, MODRON_TEST_HOOKS: "1" });
const B = await startDev("legacy", {});
const C = await startDev("hooks-off", { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST });
const sockets = [];
try {
  // ---------------- instance A: portal on ----------------
  const a = A.base;
  const author = { "cf-connecting-ip": ipBad };

  // Sign-in and rate limit: 5 attempts per minute per IP, the 6th is 429.
  const badStatus = [];
  for (let i = 0; i < 5; i++) {
    const r = await fetch(a + "/auth", json({ token: "wrong-" + i }, { headers: author }));
    badStatus.push(r.status);
  }
  check("wrong owner token is 401 (x5)", badStatus.every((s) => s === 401), badStatus.join(","));
  const sixth = await fetch(a + "/auth", json({ token: OWNER }, { headers: author }));
  check("sixth attempt in the minute is 429 even with the right token", sixth.status === 429, "status " + sixth.status);

  const good = await fetch(a + "/auth", json({ token: OWNER }, { headers: { "cf-connecting-ip": ipGood } }));
  const setCookie = good.headers.getSetCookie().find((c) => c.startsWith("modron_owner=")) || "";
  const cm = /^modron_owner=([0-9a-f]{64});/.exec(setCookie);
  check("good sign-in is 200 and sets a 32-byte hex cookie", good.status === 200 && cm !== null, "status " + good.status);
  check("cookie is HttpOnly, SameSite=Strict, Path=/, Max-Age=2592000",
    setCookie.includes("HttpOnly") && setCookie.includes("SameSite=Strict") &&
    setCookie.includes("Path=/") && setCookie.includes("Max-Age=2592000"), setCookie);
  check("cookie has no Secure flag on local http://127.0.0.1", !setCookie.includes("Secure"), setCookie);
  const cookie = cm ? "modron_owner=" + cm[1] : "";
  const withCookie = { cookie };

  const extra = await fetch(a + "/auth", json({ token: OWNER, extra: 1 }, { headers: { "cf-connecting-ip": ipGood } }));
  check("unknown field in /auth body is 400", extra.status === 400, "status " + extra.status);

  const noCookie = await fetch(a + "/api/sessions");
  check("GET /api/sessions without cookie is 401", noCookie.status === 401, "status " + noCookie.status);
  const cookieList = await fetch(a + "/api/sessions", { headers: withCookie });
  check("GET /api/sessions with cookie is 200 (empty)", cookieList.status === 200 && (await cookieList.json()).length === 0);

  // Host registration: HOST_TOKEN bearer only.
  const hostBody = { title: "demo session", host_label: "laptop", host_kind: "local", viewer_key: viewerKey, state: "idle" };
  const hostPath = a + "/api/host/sessions/" + sid;
  const hostNoAuth = await fetch(hostPath, json(hostBody, { method: "PUT" }));
  check("host PUT without bearer is 401", hostNoAuth.status === 401, "status " + hostNoAuth.status);
  const hostWrong = await fetch(hostPath, json(hostBody, { method: "PUT", headers: { authorization: "Bearer nope" } }));
  check("host PUT with wrong bearer is 401", hostWrong.status === 401, "status " + hostWrong.status);
  const hostAuth = { authorization: "Bearer " + HOST };
  const badKind = await fetch(hostPath, json({ ...hostBody, host_kind: "moon" }, { method: "PUT", headers: hostAuth }));
  check("host PUT rejects a bad host_kind (400)", badKind.status === 400, "status " + badKind.status);
  const badField = await fetch(hostPath, json({ ...hostBody, owner: true }, { method: "PUT", headers: hostAuth }));
  check("host PUT rejects an unknown field (400)", badField.status === 400, "status " + badField.status);
  const hostOk = await fetch(hostPath, json(hostBody, { method: "PUT", headers: hostAuth }));
  check("host PUT with HOST_TOKEN registers (204)", hostOk.status === 204, "status " + hostOk.status);
  const sid2 = randomUUID();
  const vk2 = createHash("sha256").update("second:" + sid2).digest("hex").slice(0, 32);
  await fetch(a + "/api/host/sessions/" + sid2, json({ ...hostBody, title: "second", viewer_key: vk2 }, { method: "PUT", headers: hostAuth }));
  const listed = await (await fetch(a + "/api/sessions", { headers: withCookie })).json();
  check("/api/sessions lists the registered session with its viewer key",
    listed.length === 2 && listed[1].id === sid && listed[1].viewer_key === viewerKey && listed[1].title === "demo session",
    JSON.stringify(listed));
  check("/api/sessions is newest first", listed[0].id === sid2, "first is " + listed[0]?.id);

  // Row shape: updated_at is ISO 8601, online is a bool, open_url is the app path.
  const fresh = listed.find((r) => r.id === sid);
  check("fresh heartbeat row is online with its own state and ISO updated_at",
    fresh && fresh.online === true && fresh.state === "idle" && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(fresh.updated_at),
    JSON.stringify(fresh));
  const innerOf = (row) => decodeURIComponent(row.open_url.slice("/app/?url=".length));
  check("open_url is /app/?url=<encoded ws url of /ws/<viewer key>>",
    fresh && fresh.open_url.startsWith("/app/?url=") &&
    innerOf(fresh) === `ws://127.0.0.1:${A.port}/ws/${viewerKey}`, fresh && fresh.open_url);

  // Staleness: a session last seen 6 minutes ago is offline, and the list's
  // clock (x-test-now) sets the threshold: 5 minutes is still online, 5 min 1 ms is not.
  const staleSid = randomUUID();
  const staleVk = createHash("sha256").update("stale:" + staleSid).digest("hex").slice(0, 32);
  const staleAt = Date.now() - 6 * MIN;
  const putStale = await fetch(a + "/api/host/sessions/" + staleSid, json(
    { ...hostBody, title: "stale", viewer_key: staleVk },
    { method: "PUT", headers: { ...hostAuth, "x-test-now": String(staleAt) } }));
  check("host PUT with a backdated x-test-now is 204 under the hook", putStale.status === 204, "status " + putStale.status);
  const listAt = async (ms) => (await fetch(a + "/api/sessions", { headers: { ...withCookie, "x-test-now": String(ms) } })).json();
  const rowOf = (rows, id) => rows.find((r) => r.id === id);
  const stale6 = rowOf(await listAt(Date.now()), staleSid);
  check("a session 6 minutes without a heartbeat is offline",
    stale6 && stale6.online === false && stale6.state === "offline" && stale6.updated_at === new Date(staleAt).toISOString(),
    JSON.stringify(stale6));
  const at4 = rowOf(await listAt(staleAt + 4 * MIN), staleSid);
  check("4 minutes after the last heartbeat it is still online", at4 && at4.online === true && at4.state === "idle");
  const at5 = rowOf(await listAt(staleAt + 5 * MIN), staleSid);
  check("exactly 5 minutes after the last heartbeat it is still online", at5 && at5.online === true);
  const at5p = rowOf(await listAt(staleAt + 5 * MIN + 1), staleSid);
  check("5 minutes and 1 ms after the last heartbeat it is offline", at5p && at5p.online === false && at5p.state === "offline");
  // A new heartbeat brings it back online with its new state.
  await fetch(a + "/api/host/sessions/" + staleSid, json(
    { ...hostBody, title: "stale", viewer_key: staleVk, state: "running" }, { method: "PUT", headers: hostAuth }));
  const revived = rowOf(await listAt(Date.now()), staleSid);
  check("a fresh heartbeat makes an offline session online again", revived && revived.online === true && revived.state === "running");
  await fetch(a + "/api/host/sessions/" + staleSid, { method: "DELETE", headers: hostAuth });

  // GC: a registration older than 30 days is removed by the next access.
  const oldSid = randomUUID();
  const oldPut = await fetch(a + "/api/host/sessions/" + oldSid, json(
    { ...hostBody, title: "ancient", viewer_key: createHash("sha256").update("old:" + oldSid).digest("hex").slice(0, 32) },
    { method: "PUT", headers: { ...hostAuth, "x-test-now": String(Date.now() - 31 * DAY) } }));
  check("a 31-day-old registration is accepted (204) before the sweep", oldPut.status === 204, "status " + oldPut.status);
  const afterSweep = await (await fetch(a + "/api/sessions", { headers: withCookie })).json();
  check("the next /api/sessions read removes a registration older than 30 days", !afterSweep.some((r) => r.id === oldSid));
  const reput = await fetch(a + "/api/host/sessions/" + sid, json(hostBody, { method: "PUT", headers: hostAuth }));
  check("current registrations survive the sweep", reput.status === 204 && afterSweep.some((r) => r.id === sid));

  // /api/me: 200 with the owner cookie, 401 without it.
  const meNo = await fetch(a + "/api/me");
  check("GET /api/me without a cookie is 401", meNo.status === 401, "status " + meNo.status);
  const meYes = await fetch(a + "/api/me", { headers: withCookie });
  check("GET /api/me with the owner cookie is 200 {owner: true}",
    meYes.status === 200 && JSON.stringify(await meYes.json()) === '{"owner":true}');

  // Root: with OWNER_TOKEN the root is a redirect to the app.
  const rootA = await fetch(a + "/", { redirect: "manual" });
  check("GET / with OWNER_TOKEN set is 302 to /app/", rootA.status === 302 && rootA.headers.get("location") === "/app/",
    "status " + rootA.status + " location " + rootA.headers.get("location"));

  // Invite links.
  const invUrl = a + "/api/invites";
  const invNoCookie = await fetch(invUrl, json({ session_id: sid, role: "viewer", ttl_hours: 2 }));
  check("POST /api/invites without cookie is 401", invNoCookie.status === 401, "status " + invNoCookie.status);
  const invMissing = await fetch(invUrl, json({ session_id: randomUUID(), role: "viewer", ttl_hours: 2 }, { headers: withCookie }));
  check("invite for an unregistered session is 404", invMissing.status === 404, "status " + invMissing.status);
  const invBadTtl = await fetch(invUrl, json({ session_id: sid, role: "viewer", ttl_hours: 0 }, { headers: withCookie }));
  check("ttl_hours 0 is 400", invBadTtl.status === 400, "status " + invBadTtl.status);
  const invBadRole = await fetch(invUrl, json({ session_id: sid, role: "owner", ttl_hours: 1 }, { headers: withCookie }));
  check("role owner cannot be granted by invite (400)", invBadRole.status === 400, "status " + invBadRole.status);

  const mintInvite = async (body) => {
    const r = await fetch(invUrl, json(body, { headers: withCookie }));
    return { status: r.status, ...(r.status === 201 ? await r.json() : {}) };
  };
  const viewerInv = await mintInvite({ session_id: sid, role: "viewer", ttl_hours: 2 });
  const approverInv = await mintInvite({ session_id: sid, role: "approver", ttl_hours: 2 });
  const revokeInv = await mintInvite({ session_id: sid, role: "viewer", ttl_hours: 2 });
  const expiringInv = await mintInvite({ session_id: sid, role: "viewer", ttl_seconds: 1 });
  check("invites are created (201) with a /i/<id>.<sig> url",
    [viewerInv, approverInv, revokeInv, expiringInv].every((i) => i.status === 201 && /\/i\/[0-9a-f]{32}\.[0-9a-f]{64}$/.test(i.url)));

  const listedInv = await (await fetch(invUrl + "?session_id=" + sid, { headers: withCookie })).json();
  check("GET /api/invites?session_id lists the invites", listedInv.length === 4 && listedInv.some((i) => i.id === viewerInv.id));
  const revokeRes = await fetch(invUrl + "/" + revokeInv.id, { method: "DELETE", headers: withCookie });
  check("DELETE /api/invites/<id> revokes (204)", revokeRes.status === 204, "status " + revokeRes.status);
  const revokeAgain = await fetch(invUrl + "/" + revokeInv.id, { method: "DELETE", headers: withCookie });
  check("a repeat DELETE of a revoked invite is harmless (204)", revokeAgain.status === 204, "status " + revokeAgain.status);

  // Signature checks.
  const vTok = viewerInv.url.slice(viewerInv.url.indexOf("/i/") + 3);
  const flipped = vTok.slice(0, -1) + (vTok.endsWith("0") ? "1" : "0");
  const tampered = await fetch(a + "/i/" + flipped, { redirect: "manual" });
  check("tampered invite signature is 403", tampered.status === 403, "status " + tampered.status);
  const revokedLink = await fetch(revokeInv.url, { redirect: "manual" });
  check("revoked invite is 410", revokedLink.status === 410, "status " + revokedLink.status);
  check("invalid invite page says only 'invite is not valid'",
    (await tampered.text()).includes("invite is not valid"));

  // Redeem the viewer invite for a ticket: 302 to the app with the inner url.
  const redeem = await fetch(viewerInv.url, { redirect: "manual" });
  const loc = redeem.headers.get("location") || "";
  const inner = decodeURIComponent(loc.slice("/app/?url=".length));
  const ticketMatch = new RegExp(`^ws://127\\.0\\.0\\.1:${A.port}/ws/${viewerKey}\\?ticket=([0-9a-f]{64})$`).exec(inner);
  check("valid invite is 302 to /app/ with an encoded ws url and a 64-hex ticket",
    redeem.status === 302 && loc.startsWith("/app/?url=") && ticketMatch !== null, "status " + redeem.status + " loc " + loc);
  const viewerTicket = ticketMatch ? ticketMatch[1] : "";
  const approverTicket = await (async () => {
    const r = await fetch(approverInv.url, { redirect: "manual" });
    const m = /ticket=([0-9a-f]{64})/.exec(decodeURIComponent(r.headers.get("location") || ""));
    return m ? m[1] : "";
  })();

  // Expiry: a 1-second invite is valid at once and dead after it lapses.
  const expiryNow = await fetch(expiringInv.url, { redirect: "manual" });
  check("a 1-second invite works before it expires", expiryNow.status === 302, "status " + expiryNow.status);
  await sleep(2200);
  const expired = await fetch(expiringInv.url, { redirect: "manual" });
  check("an expired invite is 410", expired.status === 410, "status " + expired.status);

  // The agent tunnel: HOST_TOKEN is checked before any Durable Object.
  const agentUrl = `ws://127.0.0.1:${A.port}/agent/${agentKey}`;
  const agentNoAuth = await fetch(`${a}/agent/${agentKey}`);
  check("agent tunnel without bearer is 401", agentNoAuth.status === 401, "status " + agentNoAuth.status);
  const agentWithAuth = await fetch(`${a}/agent/${agentKey}`, { headers: hostAuth });
  check("agent tunnel with bearer gets past the gate", agentWithAuth.status !== 401, "status " + agentWithAuth.status);
  const agent = connect(agentUrl, hostAuth);
  sockets.push(agent.ws);
  check("agent socket with HOST_TOKEN bearer connects", await agent.opened);
  const agentFrames = () => agent.got.map((s) => JSON.parse(s));

  // Viewer ticket: single use, then role enforcement.
  const ticketUrl = `ws://127.0.0.1:${A.port}/ws/${viewerKey}?ticket=${viewerTicket}`;
  const first = connect(ticketUrl);
  sockets.push(first.ws);
  check("viewer socket with a fresh ticket opens", await first.opened);
  const replay = connect(ticketUrl);
  sockets.push(replay.ws);
  check("a ticket is single-use (second connect refused)", !(await replay.opened));

  const v = first;
  v.ws.send(JSON.stringify({ type: "submit", id: 5, task: "should not pass" }));
  const gotForbidden = await waitUntil(() => v.got.some((s) => s.includes('"forbidden"')));
  const err = gotForbidden ? JSON.parse(v.got.find((s) => s.includes('"forbidden"'))) : {};
  check("viewer submit is answered with a forbidden error frame",
    err.type === "error" && err.kind === "forbidden" && err.message === "not permitted for this invite" && err.id === 5,
    JSON.stringify(err));
  v.ws.send("not json at all");
  v.ws.send(JSON.stringify({ type: "ping", id: 6 }));
  const pingSeen = await waitUntil(() => agentFrames().some((f) => f.type === "ping" && f.id === 6));
  check("viewer ping is relayed to the agent", pingSeen);
  check("viewer submit never reaches the agent", !agentFrames().some((f) => f.type === "submit" && f.id === 5));
  check("malformed viewer frame is dropped, not forwarded (no error reply either)",
    !agent.got.includes("not json at all") && v.got.filter((s) => s.includes('"error"')).length === 1);

  // Approver: approve passes, submit does not.
  const ap = connect(`ws://127.0.0.1:${A.port}/ws/${viewerKey}?ticket=${approverTicket}`);
  sockets.push(ap.ws);
  check("approver socket with a ticket opens", await ap.opened);
  ap.ws.send(JSON.stringify({ type: "approve", id: 7, request_id: "r1", decision: "allow_once" }));
  ap.ws.send(JSON.stringify({ type: "submit", id: 8, task: "nope" }));
  const approveSeen = await waitUntil(() => agentFrames().some((f) => f.type === "approve" && f.id === 7));
  const approverErr = await waitUntil(() => ap.got.some((s) => s.includes('"forbidden"') && s.includes('"id":8')));
  check("approver approve is relayed", approveSeen);
  check("approver submit is refused with a forbidden error", approverErr && !agentFrames().some((f) => f.type === "submit" && f.id === 8));

  // Owner cookie: unfiltered.
  const owner = connect(`ws://127.0.0.1:${A.port}/ws/${viewerKey}`, withCookie);
  sockets.push(owner.ws);
  check("owner-cookie socket opens without a ticket", await owner.opened);
  owner.ws.send(JSON.stringify({ type: "submit", id: 9, task: "owner may submit" }));
  check("owner submit reaches the agent", await waitUntil(() => agentFrames().some((f) => f.type === "submit" && f.id === 9)));

  // open_url: the owner's browser opens the inner ws url on the same origin,
  // so the cookie rides on the upgrade. That socket is unfiltered.
  const openInner = innerOf(listed.find((r) => r.id === sid));
  const openOwner = connect(openInner, withCookie);
  sockets.push(openOwner.ws);
  check("the open_url socket with the owner cookie opens", await openOwner.opened);
  openOwner.ws.send(JSON.stringify({ type: "submit", id: 10, task: "from open_url" }));
  check("a submit on the open_url socket is relayed to the agent unfiltered",
    await waitUntil(() => agentFrames().some((f) => f.type === "submit" && f.id === 10)));
  const openNoCookie = connect(openInner);
  sockets.push(openNoCookie.ws);
  check("the same open_url without the cookie is refused", !(await openNoCookie.opened));
  const openHttp = await fetch(openInner.replace(/^ws:/, "http:"));
  check("the same open_url without the cookie is 401 over HTTP", openHttp.status === 401, "status " + openHttp.status);

  // Bare share-key socket is refused once the portal is on.
  const bareHttp = await fetch(`${a}/ws/${viewerKey}`);
  check("bare /ws/<key> with no cookie or ticket is 401", bareHttp.status === 401, "status " + bareHttp.status);
  const bareWs = connect(`ws://127.0.0.1:${A.port}/ws/${viewerKey}`);
  sockets.push(bareWs.ws);
  check("bare /ws/<key> socket is refused", !(await bareWs.opened));
  const zeroTicket = await fetch(`${a}/ws/${viewerKey}?ticket=${zeros}`);
  check("an unknown ticket is 403", zeroTicket.status === 403, "status " + zeroTicket.status);

  // Revocation and session removal take effect at the next redeem.
  const gone = await fetch(`${a}/api/host/sessions/${sid}`, { method: "DELETE", headers: hostAuth });
  check("host DELETE removes the session (204)", gone.status === 204);
  const approverAfter = await fetch(approverInv.url, { redirect: "manual" });
  check("an invite for a removed session is 410", approverAfter.status === 410, "status " + approverAfter.status);

  // Sign-out revokes the cookie.
  const out = await fetch(a + "/auth/logout", { method: "POST", headers: withCookie });
  check("logout is 204 and clears the cookie", out.status === 204 && (out.headers.get("set-cookie") || "").includes("Max-Age=0"));
  const afterOut = await fetch(a + "/api/sessions", { headers: withCookie });
  check("a revoked cookie is 401", afterOut.status === 401, "status " + afterOut.status);

  // ---------------- instance B: no portal secrets (legacy) ----------------
  const b = B.base;
  const offStatus = [
    (await fetch(b + "/api/sessions")).status,
    (await fetch(b + "/auth", json({ token: "x" }))).status,
    (await fetch(b + "/i/" + "a".repeat(32) + "." + "b".repeat(64), { redirect: "manual" })).status,
    (await fetch(b + "/api/host/sessions/" + sid, json(hostBody, { method: "PUT", headers: hostAuth }))).status,
    (await fetch(b + "/api/me")).status,
  ];
  check("portal routes answer 503 without OWNER_TOKEN", offStatus.every((s) => s === 503), offStatus.join(","));
  const rootB = await fetch(b + "/", { redirect: "manual" });
  check("GET / without OWNER_TOKEN keeps the plain text answer",
    rootB.status === 200 && (await rootB.text()).startsWith("modron remote-control bridge"));
  const legacyAgentHttp = await fetch(`${b}/agent/${agentKey}`);
  check("agent tunnel without HOST_TOKEN is not gated", legacyAgentHttp.status !== 401, "status " + legacyAgentHttp.status);
  const share = await fetch(`${b}/s/${viewerKey}`, { redirect: "manual" });
  check("legacy /s/<key> still redirects to the app", share.status === 302);
  const legacyAgent = connect(`ws://127.0.0.1:${B.port}/agent/${agentKey}`);
  sockets.push(legacyAgent.ws);
  check("legacy agent connects without a bearer", await legacyAgent.opened);
  const legacyViewer = connect(`ws://127.0.0.1:${B.port}/ws/${viewerKey}`);
  sockets.push(legacyViewer.ws);
  check("legacy viewer connects with the share key alone", await legacyViewer.opened);
  legacyViewer.ws.send(JSON.stringify({ type: "submit", id: 1, task: "legacy" }));
  check("legacy viewer submit is relayed unfiltered",
    await waitUntil(() => legacyAgent.got.some((s) => s.includes('"task":"legacy"'))));
  const legacyTicket = await fetch(`${b}/ws/${viewerKey}?ticket=${zeros}`);
  check("a ticket is refused when the portal is off (403)", legacyTicket.status === 403, "status " + legacyTicket.status);
  // ---------------- instance C: portal on, test hooks OFF ----------------
  // The clock header and the ttl_seconds field must do nothing here.
  const c = C.base;
  const cSignIn = await fetch(c + "/auth", json({ token: OWNER }, { headers: { "cf-connecting-ip": ipHooksOff } }));
  const cCookieLine = cSignIn.headers.getSetCookie().find((x) => x.startsWith("modron_owner=")) || "";
  const cCookie = { cookie: cCookieLine.split(";")[0] };
  check("instance C signs in", cSignIn.status === 200 && cCookie.cookie.length > "modron_owner=".length);
  const cMe = await fetch(c + "/api/me", { headers: cCookie });
  check("instance C GET /api/me is 200 with the cookie", cMe.status === 200);
  const cSid = randomUUID();
  const cVk = createHash("sha256").update("c:" + cSid).digest("hex").slice(0, 32);
  const cPut = await fetch(c + "/api/host/sessions/" + cSid, json(
    { ...hostBody, title: "hooks off", viewer_key: cVk },
    { method: "PUT", headers: { ...hostAuth, "x-test-now": String(Date.now() - 31 * DAY) } }));
  check("instance C registers a session (204)", cPut.status === 204, "status " + cPut.status);
  const cRows = await (await fetch(c + "/api/sessions", { headers: { ...cCookie, "x-test-now": String(Date.now() - 31 * DAY) } })).json();
  const cRow = cRows.find((r) => r.id === cSid);
  check("without test hooks x-test-now is ignored: the row is online and was not swept",
    cRow !== undefined && cRow.online === true, JSON.stringify(cRow));
  const cTtl = await fetch(c + "/api/invites", json({ session_id: cSid, role: "viewer", ttl_seconds: 1 }, { headers: cCookie }));
  check("without test hooks ttl_seconds is refused (400)", cTtl.status === 400, "status " + cTtl.status);
  const cBadClock = await fetch(c + "/api/sessions", { headers: { ...cCookie, "x-test-now": "abc" } });
  check("a non-numeric x-test-now is ignored (200)", cBadClock.status === 200);
} catch (err) {
  check("portal test run", false, String(err && err.stack ? err.stack : err).split("\n").slice(0, 4).join(" | "));
} finally {
  for (const s of sockets) { try { s.close(); } catch { /* closed */ } }
  A.stop();
  B.stop();
  C.stop();
  await sleep(500);
  rmSync(stateRoot, { recursive: true, force: true });
  if (madePlaceholder) rmSync(appDir, { recursive: true, force: true });
}

console.log(failures.length ? `${failures.length} check(s) failed` : "all portal checks passed");
process.exit(failures.length ? 1 : 0);
