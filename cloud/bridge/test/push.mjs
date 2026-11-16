// Web Push checks against real local Workers (`wrangler dev --local`, no
// Cloudflare account). Two instances:
//   A: OWNER_TOKEN, HOST_TOKEN, VAPID keys, PUSH_TEST_ORIGIN pointing at a
//      fake push service on 127.0.0.1, MODRON_TEST_HOOKS=1.
//   B: OWNER_TOKEN and HOST_TOKEN only (no VAPID: the push routes answer 503).
// A fake agent socket sends Notify frames; the fake push service records
// every POST and answers 201, or 410 for /gone/ paths. Payloads are decrypted
// with node:crypto (not the code under test). Run from cloud/bridge:
//   node test/push.mjs
import { spawn } from "node:child_process";
import { createDecipheriv, createECDH, createHash, createPublicKey, hkdfSync, randomBytes, randomUUID, webcrypto } from "node:crypto";
import { createServer } from "node:http";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer as netServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const bridgeDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const appDir = join(bridgeDir, "public", "app");
const stateRoot = join(bridgeDir, ".wrangler", "push-test-" + randomBytes(6).toString("hex"));
const subtle = webcrypto.subtle;
const failures = [];
const check = (name, ok, detail = "") => {
  console.log((ok ? "PASS " : "FAIL ") + name + (ok || !detail ? "" : " -- " + detail));
  if (!ok) failures.push(name);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitUntil(pred, ms = 8000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (pred()) return true;
    await sleep(50);
  }
  return pred();
}
const b64u = (buf) => Buffer.from(buf).toString("base64url");

// Placeholder bundle, only when the real one (sync-app.sh) is absent.
let madePlaceholder = false;
if (!existsSync(join(appDir, "index.html"))) {
  mkdirSync(appDir, { recursive: true });
  writeFileSync(join(appDir, "index.html"), "<!doctype html><title>placeholder</title>\n");
  madePlaceholder = true;
}

const freePort = () => new Promise((resolve, reject) => {
  const srv = netServer();
  srv.unref();
  srv.on("error", reject);
  srv.listen(0, "127.0.0.1", () => { const { port } = srv.address(); srv.close(() => resolve(port)); });
});

async function startDev(label, vars) {
  const port = await freePort();
  const args = [
    "wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(port),
    "--show-interactive-dev-session=false", "--persist-to", join(stateRoot, label),
  ];
  for (const [name, value] of Object.entries(vars)) args.push("--var", `${name}:${value}`);
  const out = [];
  // Own process group, so stop() also ends wrangler's node and workerd children.
  const child = spawn("npx", args, { cwd: bridgeDir, detached: true, env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" } });
  child.stdout.on("data", (d) => out.push(String(d)));
  child.stderr.on("data", (d) => out.push(String(d)));
  const base = `http://127.0.0.1:${port}`;
  let ready = false;
  for (let end = Date.now() + 90000; Date.now() < end && !ready;) {
    try { await fetch(base + "/", { redirect: "manual" }); ready = true; } catch { await sleep(300); }
  }
  if (!ready) {
    child.kill("SIGTERM");
    throw new Error(`${label}: wrangler dev did not become ready:\n${out.join("")}`);
  }
  return { base, port, log: out, stop: () => { try { process.kill(-child.pid, "SIGTERM"); } catch { /* already gone */ } } };
}

// --- keys ---

async function uaKeys() {
  const pair = await subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
  const pub = Buffer.from(await subtle.exportKey("raw", pair.publicKey));
  const jwk = await subtle.exportKey("jwk", pair.privateKey);
  return { pub, d: jwk.d, auth: randomBytes(16) };
}

async function vapidKeys() {
  const pair = await subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  const pub = Buffer.from(await subtle.exportKey("raw", pair.publicKey));
  const jwk = await subtle.exportKey("jwk", pair.privateKey);
  return { pub: b64u(pub), d: jwk.d, subject: "https://modron.example.invalid" };
}

// Independent aes128gcm decryption (RFC 8291 / RFC 8188) with node:crypto.
function decrypt(body, ua) {
  const idlen = body[20];
  const asPub = body.subarray(21, 21 + idlen);
  const ct = body.subarray(21 + idlen);
  const ecdh = createECDH("prime256v1");
  ecdh.setPrivateKey(Buffer.from(ua.d, "base64url"));
  const shared = ecdh.computeSecret(asPub);
  const keyInfo = Buffer.concat([Buffer.from("WebPush: info\0"), ua.pub, asPub]);
  const ikm = Buffer.from(hkdfSync("sha256", shared, ua.auth, keyInfo, 32));
  const salt = body.subarray(0, 16);
  const cek = Buffer.from(hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: aes128gcm\0"), 16));
  const nonce = Buffer.from(hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: nonce\0"), 12));
  const d = createDecipheriv("aes-128-gcm", cek, nonce);
  d.setAuthTag(ct.subarray(ct.length - 16));
  const padded = Buffer.concat([d.update(ct.subarray(0, ct.length - 16)), d.final()]);
  return padded.subarray(0, padded.length - 1).toString("utf8");
}

async function verifyVapid(header, vapid, audience) {
  const m = /^vapid t=([^,]+), k=(.+)$/.exec(header || "");
  if (!m) return "bad authorization header";
  const [h, p, s] = m[1].split(".");
  const claims = JSON.parse(Buffer.from(p, "base64url").toString());
  if (claims.aud !== audience) return "aud " + claims.aud;
  if (claims.sub !== vapid.subject) return "sub " + claims.sub;
  if (claims.exp - Math.floor(Date.now() / 1000) > 24 * 3600) return "exp too far";
  const x = Buffer.from(vapid.pub, "base64url").subarray(1, 33).toString("base64url");
  const y = Buffer.from(vapid.pub, "base64url").subarray(33, 65).toString("base64url");
  const pub = await subtle.importKey("jwk", { kty: "EC", crv: "P-256", x, y, ext: true }, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
  const ok = await subtle.verify({ name: "ECDSA", hash: "SHA-256" }, pub, Buffer.from(s, "base64url"), Buffer.from(h + "." + p));
  return ok ? null : "signature does not verify";
}

// --- fake push service ---

const pushLog = [];
const fake = createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    pushLog.push({ path: req.url, headers: req.headers, body: Buffer.concat(chunks) });
    res.writeHead(req.url.startsWith("/gone/") ? 410 : 201);
    res.end();
  });
});
await new Promise((r) => fake.listen(0, "127.0.0.1", r));
const fakeOrigin = `http://127.0.0.1:${fake.address().port}`;

// --- run ---

const OWNER = randomBytes(24).toString("hex");
const HOST = randomBytes(24).toString("hex");
const vapid = await vapidKeys();
const agentKey = randomBytes(16).toString("hex");
const viewerKey = createHash("sha256").update("modron-viewer:" + agentKey).digest("hex").slice(0, 32);
const sid = randomUUID();
const bearer = { authorization: "Bearer " + HOST };
const ip = "198.51.100.71";
const json = (body, headers = {}) => ({ method: "POST", headers: { "content-type": "application/json", ...headers }, body: JSON.stringify(body) });
const sockets = [];
let A, B;
try {
  A = await startDev("push", {
    OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, MODRON_TEST_HOOKS: "1",
    VAPID_PUBLIC_KEY: vapid.pub, VAPID_PRIVATE_KEY: vapid.d, VAPID_SUBJECT: vapid.subject,
    PUSH_TEST_ORIGIN: fakeOrigin,
  });
  B = await startDev("push-off", { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST });

  // ---- instance A ----
  const a = A.base;
  const sw = await fetch(a + "/app/sw.js");
  const swText = await sw.text();
  check("the service worker is served at /app/sw.js", sw.status === 200 && /javascript/.test(sw.headers.get("content-type") || "") && swText.includes('addEventListener("push"'), String(sw.status));
  const r0 = await fetch(a + "/api/push/key");
  check("key route without a cookie is 401", r0.status === 401, String(r0.status));
  const signin = await fetch(a + "/auth", json({ token: OWNER }, { "cf-connecting-ip": ip }));
  const cookie = (signin.headers.get("set-cookie") || "").split(";")[0];
  check("owner signs in", signin.status === 200 && cookie.startsWith("modron_owner="), String(signin.status));
  const mine = { cookie };

  const key = await fetch(a + "/api/push/key", { headers: mine });
  const keyBody = key.status === 200 ? await key.json() : {};
  check("key route answers the VAPID public key to the owner", key.status === 200 && keyBody.key === vapid.pub, String(key.status));

  const owner = { ua: await uaKeys() };
  const subBody = (endpoint, ua, extra = {}) => ({
    endpoint,
    keys: { p256dh: ua.pub.toString("base64url"), auth: ua.auth.toString("base64url") },
    ...extra,
  });
  const noCookie = await fetch(a + "/api/push/subscribe", json(subBody(fakeOrigin + "/push/x", owner.ua)));
  check("subscribe without a cookie or invite is 401", noCookie.status === 401, String(noCookie.status));
  const evil = await fetch(a + "/api/push/subscribe", json(subBody("https://evil.example/push", owner.ua), mine));
  check("subscribe to an unknown push host is 400", evil.status === 400, String(evil.status));
  const local = await fetch(a + "/api/push/subscribe", json(subBody("http://localhost:" + fake.address().port + "/push/x", owner.ua), mine));
  check("subscribe to a non-test http origin is 400", local.status === 400, String(local.status));
  const ownerSub = await fetch(a + "/api/push/subscribe", json(subBody(fakeOrigin + "/push/owner", owner.ua), mine));
  check("owner subscribes (204)", ownerSub.status === 204, String(ownerSub.status));

  const reg = await fetch(a + "/api/host/sessions/" + sid, {
    method: "PUT", headers: { ...bearer, "content-type": "application/json" },
    body: JSON.stringify({ title: "Demo session", host_label: "test", host_kind: "local", viewer_key: viewerKey, state: "idle" }),
  });
  check("host registers the session (204)", reg.status === 204, String(reg.status));

  const inviteOf = async (role) => {
    const r = await fetch(a + "/api/invites", json({ session_id: sid, role, ttl_hours: 1 }, mine));
    const url = (await r.json()).url;
    return url.slice(url.indexOf("/i/") + 3);
  };
  const viewerToken = await inviteOf("viewer");
  const approverToken = await inviteOf("approver");
  const approver = { ua: await uaKeys() };
  const viewerTry = await fetch(a + "/api/push/subscribe", json(subBody(fakeOrigin + "/push/viewer", approver.ua, { invite: viewerToken })));
  check("a viewer invite may not subscribe (403)", viewerTry.status === 403, String(viewerTry.status));
  const approverSub = await fetch(a + "/api/push/subscribe", json(subBody(fakeOrigin + "/push/approver", approver.ua, { invite: approverToken })));
  check("an approver invite subscribes for its session (204)", approverSub.status === 204, String(approverSub.status));

  const agent = new WebSocket(`ws://127.0.0.1:${A.port}/agent/${agentKey}`, { headers: bearer });
  sockets.push(agent);
  const agentOpen = await waitUntil(() => agent.readyState === 1, 10000);
  check("fake agent tunnel opens with the host bearer", agentOpen);
  const notify = (kind, text) => agent.send(JSON.stringify({ type: "notify", session: sid, seq: 1, kind, text }));
  const url = `${A.base}/app/?url=${encodeURIComponent(`ws://127.0.0.1:${A.port}/ws/${viewerKey}`)}`;

  notify("approval", "approval needed: bash");
  const got2 = await waitUntil(() => pushLog.length >= 2, 10000);
  check("an approval Notify reaches the owner and approver endpoints", got2 && pushLog.length === 2, "posts " + pushLog.length);
  const byPath = (p) => pushLog.find((x) => x.path === p);
  const ownerPost = byPath("/push/owner");
  const approverPost = byPath("/push/approver");
  if (ownerPost) {
    const payload = JSON.parse(decrypt(ownerPost.body, owner.ua));
    check("owner payload: session title, the Notify text, the portal URL",
      payload.title === "Demo session" && payload.body === "approval needed: bash" && payload.data?.url === url,
      JSON.stringify(payload));
    check("owner push is encrypted with aes128gcm and carries TTL", ownerPost.headers["content-encoding"] === "aes128gcm" && ownerPost.headers.ttl === "300");
    check("owner push carries a verifiable VAPID JWT", (await verifyVapid(ownerPost.headers.authorization, vapid, fakeOrigin)) === null,
      await verifyVapid(ownerPost.headers.authorization, vapid, fakeOrigin));
  } else check("owner payload: session title, the Notify text, the portal URL", false, "no owner post");
  if (approverPost) {
    const payload = JSON.parse(decrypt(approverPost.body, approver.ua));
    check("approver payload matches the Notify text", payload.body === "approval needed: bash" && payload.title === "Demo session", JSON.stringify(payload));
  } else check("approver payload matches the Notify text", false, "no approver post");

  notify("approval", "approval needed: write");
  await sleep(1500);
  check("a second approval within 10 s is rate limited", pushLog.length === 2, "posts " + pushLog.length);

  notify("done", "done: Demo session");
  const got4 = await waitUntil(() => pushLog.length >= 4, 8000);
  check("a done Notify is a different kind and is sent", got4 && pushLog.length === 4, "posts " + pushLog.length);

  const gone = { ua: await uaKeys() };
  const goneSub = await fetch(a + "/api/push/subscribe", json(subBody(fakeOrigin + "/gone/1", gone.ua), mine));
  check("a second owner device subscribes", goneSub.status === 204, String(goneSub.status));
  notify("failed", "failed: boom");
  await waitUntil(() => pushLog.filter((x) => x.path === "/gone/1").length === 1, 8000);
  check("the push service's 410 is answered (one attempt)", pushLog.filter((x) => x.path === "/gone/1").length === 1);
  await sleep(1500); // let the removal land before the next Notify
  notify("wake", "wake: nightly review");
  await sleep(2000);
  const goneCount = pushLog.filter((x) => x.path === "/gone/1").length;
  check("a 410 removes the subscription (no second attempt)", goneCount === 1, "gone posts " + goneCount);

  const del = await fetch(a + "/api/push/subscribe", { method: "DELETE", headers: { ...mine, "content-type": "application/json" }, body: JSON.stringify({ endpoint: fakeOrigin + "/push/owner" }) });
  check("owner deletes its subscription (204)", del.status === 204, String(del.status));
  const delNoAuth = await fetch(a + "/api/push/subscribe", { method: "DELETE", headers: { "content-type": "application/json" }, body: JSON.stringify({ endpoint: fakeOrigin + "/push/approver" }) });
  check("delete without a cookie or invite is 401", delNoAuth.status === 401, String(delNoAuth.status));

  // ---- instance B: no VAPID secrets ----
  const bs = await fetch(B.base + "/auth", json({ token: OWNER }, { "cf-connecting-ip": "198.51.100.72" }));
  const bcookie = { cookie: (bs.headers.get("set-cookie") || "").split(";")[0] };
  const bKey = await fetch(B.base + "/api/push/key", { headers: bcookie });
  check("without VAPID secrets the key route answers 503", bKey.status === 503, String(bKey.status));
  const bSub = await fetch(B.base + "/api/push/subscribe", json(subBody(fakeOrigin + "/push/b", owner.ua), bcookie));
  check("without VAPID secrets subscribe answers 503", bSub.status === 503, String(bSub.status));
} catch (e) {
  console.log("FAIL harness -- " + (e && e.stack ? e.stack : e));
  failures.push("harness");
} finally {
  for (const s of sockets) { try { s.close(); } catch { /* gone */ } }
  if (A) A.stop();
  if (B) B.stop();
  fake.close();
  await sleep(500);
  rmSync(stateRoot, { recursive: true, force: true });
  if (madePlaceholder) rmSync(appDir, { recursive: true, force: true });
}
if (failures.length && A) console.log("--- instance A log tail ---\n" + A.log.join("").split("\n").slice(-40).join("\n"));
console.log(failures.length === 0 ? "ALL PASS" : `${failures.length} FAILED: ${failures.join(", ")}`);
process.exit(failures.length === 0 ? 0 : 1);
