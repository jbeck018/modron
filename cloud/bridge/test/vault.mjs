// Provider-key vault checks against real local Workers (`wrangler dev --local`,
// no Cloudflare account). Three instances:
//   A: OWNER_TOKEN, HOST_TOKEN and VAULT_KEY (the vault on).
//   B: OWNER_TOKEN and HOST_TOKEN only (VAULT_KEY unset: vault routes 503).
//   C: OWNER_TOKEN and VAULT_KEY only (HOST_TOKEN unset: host fetch 503).
// Secrets are random per run and passed as --var. The plaintext marker is
// searched for in the instance's persisted state after a write: the Directory
// must hold ciphertext. The test never prints a value. Run from cloud/bridge:
//   node test/vault.mjs
import { spawn } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const bridgeDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const appDir = join(bridgeDir, "public", "app");
const stateRoot = join(bridgeDir, ".wrangler", "vault-test-" + randomBytes(6).toString("hex"));
const failures = [];
const check = (name, ok, detail = "") => {
  console.log((ok ? "PASS " : "FAIL ") + name + (ok || !detail ? "" : " -- " + detail));
  if (!ok) failures.push(name);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const children = [];

let madePlaceholder = false;
if (!existsSync(join(appDir, "index.html"))) {
  mkdirSync(appDir, { recursive: true });
  writeFileSync(join(appDir, "index.html"), "<!doctype html><title>placeholder</title>\n");
  madePlaceholder = true;
}

const freePort = () => new Promise((resolve, reject) => {
  const srv = createServer();
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
  const child = spawn("npx", args, {
    cwd: bridgeDir, detached: true,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" },
  });
  children.push(child);
  child.stdout.on("data", (d) => out.push(String(d)));
  child.stderr.on("data", (d) => out.push(String(d)));
  const base = `http://127.0.0.1:${port}`;
  let ready = false;
  for (let end = Date.now() + 90000; Date.now() < end && !ready;) {
    try { await fetch(base + "/", { redirect: "manual" }); ready = true; } catch { await sleep(300); }
  }
  if (!ready) throw new Error(`${label}: wrangler dev did not become ready:\n${out.join("")}`);
  return { base, stop: () => { try { process.kill(-child.pid, "SIGTERM"); } catch { /* gone */ } } };
}

// Every file under dir, recursively, as one buffer list.
function filesUnder(dir) {
  const found = [];
  if (!existsSync(dir)) return found;
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) found.push(...filesUnder(p));
    else found.push(p);
  }
  return found;
}

const OWNER = randomBytes(24).toString("hex");
const HOST = randomBytes(24).toString("hex");
const VAULT = randomBytes(24).toString("hex");
const MARKER = "sk-vault-test-" + randomBytes(12).toString("hex");
let ipSeq = 0;
const ip = () => ({ "cf-connecting-ip": `198.51.100.${200 + (ipSeq++ % 50)}` });
const json = (body, headers = {}) => ({
  method: "POST", headers: { "content-type": "application/json", ...headers }, body: JSON.stringify(body),
});
const hostAuth = { authorization: `Bearer ${HOST}` };

let A, B, C;
try {
  A = await startDev("a", { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, VAULT_KEY: VAULT });
  B = await startDev("b", { OWNER_TOKEN: OWNER, HOST_TOKEN: HOST });
  C = await startDev("c", { OWNER_TOKEN: OWNER, VAULT_KEY: VAULT });

  // ---- B: no VAULT_KEY ----
  const bs = await fetch(B.base + "/auth", json({ token: OWNER }, ip()));
  const bcookie = { cookie: (bs.headers.get("set-cookie") || "").split(";")[0] };
  const bList = await fetch(B.base + "/api/credentials", { headers: bcookie });
  check("without VAULT_KEY the list answers 503 (even with a cookie)", bList.status === 503, String(bList.status));
  const bPut = await fetch(B.base + "/api/credentials/anthropic", { ...json({ key: MARKER }, { ...bcookie, origin: B.base }), method: "PUT" });
  check("without VAULT_KEY a PUT answers 503", bPut.status === 503, String(bPut.status));
  const bHost = await fetch(B.base + "/api/host/credentials", { headers: hostAuth });
  check("without VAULT_KEY the host fetch answers 503", bHost.status === 503, String(bHost.status));
  const bMe = await fetch(B.base + "/api/me", { headers: bcookie });
  check("the rest of the portal still works without VAULT_KEY", bMe.status === 200, String(bMe.status));

  // ---- C: no HOST_TOKEN ----
  const cs = await fetch(C.base + "/auth", json({ token: OWNER }, ip()));
  const ccookie = { cookie: (cs.headers.get("set-cookie") || "").split(";")[0] };
  const cHost = await fetch(C.base + "/api/host/credentials", { headers: { authorization: `Bearer ${VAULT}` } });
  check("without HOST_TOKEN the host fetch answers 503", cHost.status === 503, String(cHost.status));
  const cPut = await fetch(C.base + "/api/credentials/anthropic", { ...json({ key: MARKER }, { ...ccookie, origin: C.base }), method: "PUT" });
  check("with VAULT_KEY but no HOST_TOKEN the owner can still store a key", cPut.status === 204, String(cPut.status));

  // ---- A: the vault on ----
  const as = await fetch(A.base + "/auth", json({ token: OWNER }, ip()));
  const cookie = (as.headers.get("set-cookie") || "").split(";")[0];
  const owner = { cookie };
  check("owner signs in on A", as.status === 200, String(as.status));

  const noCookieList = await fetch(A.base + "/api/credentials");
  check("no cookie: list is 401", noCookieList.status === 401, String(noCookieList.status));
  const noCookiePut = await fetch(A.base + "/api/credentials/anthropic", { ...json({ key: MARKER }, { origin: A.base }), method: "PUT" });
  check("no cookie: PUT is 401", noCookiePut.status === 401, String(noCookiePut.status));

  // Invites: a viewer and an approver invite on a registered session.
  const sessionId = randomUUID();
  const viewerKey = randomBytes(16).toString("hex");
  const reg = await fetch(A.base + `/api/host/sessions/${sessionId}`, {
    method: "PUT", headers: { ...hostAuth, "content-type": "application/json" },
    body: JSON.stringify({ title: "vault", host_label: "test", host_kind: "local", viewer_key: viewerKey, state: "idle" }),
  });
  check("host registers a session for invites (204)", reg.status === 204, String(reg.status));
  const invite = async (role) => {
    const r = await fetch(A.base + "/api/invites", json({ session_id: sessionId, role, ttl_hours: 1 }, { ...owner, origin: A.base }));
    const body = await r.json();
    return new URL(body.url).pathname.replace(/^\/i\//, "");
  };
  const viewerToken = await invite("viewer");
  const approverToken = await invite("approver");
  for (const [label, token] of [["viewer", viewerToken], ["approver", approverToken]]) {
    const r = await fetch(A.base + `/api/credentials?invite=${encodeURIComponent(token)}`);
    check(`${label} invite cannot list (403)`, r.status === 403, String(r.status));
    const w = await fetch(A.base + `/api/credentials/anthropic?invite=${encodeURIComponent(token)}`, {
      ...json({ key: MARKER }, { origin: A.base }), method: "PUT",
    });
    check(`${label} invite cannot store a key (403)`, w.status === 403, String(w.status));
  }

  // Owner round trip. The list never contains the value.
  const put = await fetch(A.base + "/api/credentials/anthropic", { ...json({ key: MARKER }, { ...owner, origin: A.base }), method: "PUT" });
  check("owner stores a key (204)", put.status === 204, String(put.status));
  check("a vault response is no-store", put.headers.get("cache-control") === "no-store", String(put.headers.get("cache-control")));
  const list = await fetch(A.base + "/api/credentials", { headers: owner });
  const listText = await list.text();
  check("owner lists providers (200)", list.status === 200, String(list.status));
  const listed = JSON.parse(listText).credentials;
  const anth = listed.find((row) => row.provider === "anthropic");
  check("the list marks anthropic set with its env var", anth && anth.set === true && anth.env_var === "ANTHROPIC_API_KEY", JSON.stringify(anth));
  check("the list marks other providers unset", listed.filter((row) => row.provider !== "anthropic").every((row) => row.set === false));
  check("the list never contains the value", !listText.includes(MARKER));
  check("the list is no-store", list.headers.get("cache-control") === "no-store");

  // Refusals.
  const xo = await fetch(A.base + "/api/credentials/openai-compat", { ...json({ key: MARKER }, { ...owner, origin: "https://evil.example" }), method: "PUT" });
  check("cross-origin PUT is refused (403)", xo.status === 403, String(xo.status));
  const noOrigin = await fetch(A.base + "/api/credentials/openai-compat", { ...json({ key: MARKER }, owner), method: "PUT" });
  check("PUT without an Origin header is refused (403)", noOrigin.status === 403, String(noOrigin.status));
  const xoDel = await fetch(A.base + "/api/credentials/anthropic", { method: "DELETE", headers: { ...owner, origin: "https://evil.example" } });
  check("cross-origin DELETE is refused (403)", xoDel.status === 403, String(xoDel.status));
  const unknown = await fetch(A.base + "/api/credentials/bogus", { ...json({ key: MARKER }, { ...owner, origin: A.base }), method: "PUT" });
  check("unknown provider id is refused (400)", unknown.status === 400, String(unknown.status));
  const oversize = await fetch(A.base + "/api/credentials/groq", { ...json({ key: "k".repeat(513) }, { ...owner, origin: A.base }), method: "PUT" });
  check("an oversize key is refused (400)", oversize.status === 400, String(oversize.status));
  const control = await fetch(A.base + "/api/credentials/groq", { ...json({ key: "gsk\nSECOND" }, { ...owner, origin: A.base }), method: "PUT" });
  const controlText = await control.text();
  check("a key with a control character is refused (400)", control.status === 400, String(control.status));
  check("the refusal does not echo the value", !controlText.includes("gsk"));
  const notString = await fetch(A.base + "/api/credentials/groq", { ...json({ key: 12 }, { ...owner, origin: A.base }), method: "PUT" });
  check("a non-string key is refused (400)", notString.status === 400, String(notString.status));
  const extra = await fetch(A.base + "/api/credentials/groq", { ...json({ key: "gsk-x", role: "owner" }, { ...owner, origin: A.base }), method: "PUT" });
  check("an unknown body field is refused (400)", extra.status === 400, String(extra.status));

  // At rest: the value is not in the Directory's storage, and the table is.
  const files = filesUnder(join(stateRoot, "a"));
  const hit = files.find((f) => readFileSync(f).includes(MARKER));
  check("the stored value is absent from the Directory's files", hit === undefined, hit ? hit : "");
  const tableSeen = files.some((f) => readFileSync(f).includes("credentials"));
  check("the persisted state holds the credentials table (scan is real)", tableSeen);

  // Host fetch: bearer required, env-var-named map.
  const hNone = await fetch(A.base + "/api/host/credentials");
  check("host fetch without a bearer is 401", hNone.status === 401, String(hNone.status));
  const hWrong = await fetch(A.base + "/api/host/credentials", { headers: { authorization: `Bearer ${VAULT}` } });
  check("host fetch with the wrong bearer is 401", hWrong.status === 401, String(hWrong.status));
  const hOk = await fetch(A.base + "/api/host/credentials", { headers: hostAuth });
  const hBody = await hOk.json();
  check("host fetch with the bearer answers 200", hOk.status === 200, String(hOk.status));
  check("host fetch maps the value to its env var", hBody.credentials && hBody.credentials.ANTHROPIC_API_KEY === MARKER, JSON.stringify(Object.keys(hBody.credentials || {})));
  check("host fetch carries only the stored providers", Object.keys(hBody.credentials).length === 1, JSON.stringify(Object.keys(hBody.credentials)));

  // Delete: gone from the list and from the host fetch.
  const del = await fetch(A.base + "/api/credentials/anthropic", { method: "DELETE", headers: { ...owner, origin: A.base } });
  check("owner deletes the key (204)", del.status === 204, String(del.status));
  const del2 = await fetch(A.base + "/api/credentials/anthropic", { method: "DELETE", headers: { ...owner, origin: A.base } });
  check("deleting an unset key is 404", del2.status === 404, String(del2.status));
  const after = await (await fetch(A.base + "/api/host/credentials", { headers: hostAuth })).json();
  check("after delete the host fetch has no ANTHROPIC_API_KEY", !("ANTHROPIC_API_KEY" in after.credentials), JSON.stringify(after));
} catch (e) {
  console.log("FAIL harness -- " + (e && e.stack ? e.stack : e));
  failures.push("harness");
} finally {
  for (const c of children) { try { process.kill(-c.pid, "SIGTERM"); } catch { /* gone */ } }
  await sleep(500);
  rmSync(stateRoot, { recursive: true, force: true });
  if (madePlaceholder) rmSync(appDir, { recursive: true, force: true });
}
console.log(failures.length === 0 ? "ALL PASS" : `${failures.length} FAILED: ${failures.join(", ")}`);
process.exit(failures.length === 0 ? 0 : 1);
