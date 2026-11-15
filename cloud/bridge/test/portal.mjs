// Portal checks against real local Workers (`wrangler dev --local`, miniflare
// on this machine; no Cloudflare account is touched). Two instances:
//   A: OWNER_TOKEN + HOST_TOKEN set, MODRON_TEST_HOOKS=1 (expiry hook).
//   B: no portal secrets (the legacy share-link deployment).
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
      ready = (await fetch(base + "/", { redirect: "manual" })).status === 200;
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
const zeros = "0".repeat(64);

mkdirSync(stateRoot, { recursive: true });
const A = await startDev("portal", { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, MODRON_TEST_HOOKS: "1" });
const B = await startDev("legacy", {});
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
  ];
  check("portal routes answer 503 without OWNER_TOKEN", offStatus.every((s) => s === 503), offStatus.join(","));
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
} catch (err) {
  check("portal test run", false, String(err && err.message ? err.message : err));
} finally {
  for (const s of sockets) { try { s.close(); } catch { /* closed */ } }
  A.stop();
  B.stop();
  await sleep(500);
  rmSync(stateRoot, { recursive: true, force: true });
  if (madePlaceholder) rmSync(appDir, { recursive: true, force: true });
}

console.log(failures.length ? `${failures.length} check(s) failed` : "all portal checks passed");
process.exit(failures.length ? 1 : 0);
