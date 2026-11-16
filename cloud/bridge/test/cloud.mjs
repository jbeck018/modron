// GitHub accounts and cloud runs (PLAN 10.10) against real local Workers
// (`wrangler dev --local`, no Cloudflare account). The GitHub API and the
// container worker are two fakes served by this process on loopback; the
// bridge reaches them through GITHUB_API_ORIGIN and CONTAINER_URL, which are
// honoured only with MODRON_TEST_HOOKS. Nothing here calls the real GitHub.
// Three instances:
//   A: everything set (the full path).
//   B: VAULT_KEY set, CONTAINER_URL not https (runs answer 503, accounts work).
//   C: no VAULT_KEY (accounts and runs answer 503 even with a cookie).
// Run from cloud/bridge:  node test/cloud.mjs
import { spawn } from "node:child_process";
import { createServer as createHttp } from "node:http";
import { randomBytes, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const bridgeDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const appDir = join(bridgeDir, "public", "app");
const stateRoot = join(bridgeDir, ".wrangler", "cloud-test-" + randomBytes(6).toString("hex"));
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
  return { base, origin: base };
}

// --- secrets (random per run; never printed) ---
const OWNER = randomBytes(24).toString("hex");
const HOST = randomBytes(24).toString("hex");
const VAULT = randomBytes(24).toString("hex");
const WAKE = randomBytes(24).toString("hex");
const TOKEN_ALICE = "tok-alice-" + randomBytes(12).toString("hex");
const TOKEN_BOB = "tok-bob-" + randomBytes(12).toString("hex");
const TOKEN_BAD = "tok-refused-" + randomBytes(12).toString("hex");

// Every response body the browser side saw; the token must be in none of them.
const browserSeen = [];

// --- fake GitHub (loopback) ---
const GH_LOGINS = new Map([[TOKEN_ALICE, "alice"], [TOKEN_BOB, "bob"]]);
const ghRequests = [];
const REPO_TOTAL = 230;
const allRepos = Array.from({ length: REPO_TOTAL }, (_, i) => ({
  full_name: `alice/${i % 2 === 0 ? "needle" : "other"}-${i}`,
  private: i % 3 === 0,
  default_branch: "main",
  pushed_at: new Date(Date.UTC(2026, 10, 16) - i * 60000).toISOString(),
  // Extra fields GitHub sends; the bridge must not pass these on.
  html_url: "https://github.com/alice/x",
  owner: { login: "alice" },
}));
const gh = createHttp((req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");
  const auth = req.headers.authorization ?? "";
  const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
  ghRequests.push({ path: url.pathname, query: url.search, ua: req.headers["user-agent"], accept: req.headers.accept, token });
  const send = (status, body) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(body)); };
  if (!GH_LOGINS.has(token)) return send(401, { message: "Bad credentials" });
  if (url.pathname === "/user") return send(200, { login: GH_LOGINS.get(token), id: 1 });
  if (url.pathname === "/user/repos") {
    const per = Number(url.searchParams.get("per_page") ?? "30");
    const page = Number(url.searchParams.get("page") ?? "1");
    return send(200, allRepos.slice((page - 1) * per, page * per));
  }
  return send(404, { message: "Not Found" });
});
await new Promise((r) => gh.listen(0, "127.0.0.1", r));
const GH_ORIGIN = `http://127.0.0.1:${gh.address().port}`;

// --- fake container worker (loopback) ---
const ctRequests = [];
const ct = createHttp((req, res) => {
  let raw = "";
  req.on("data", (c) => { raw += c; });
  req.on("end", () => {
    const body = raw ? JSON.parse(raw) : null;
    ctRequests.push({ path: req.url, auth: req.headers.authorization ?? "", body });
    const send = (status, value) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(value)); };
    if (req.headers.authorization !== `Bearer ${WAKE}`) return send(401, { error: "unauthorized" });
    if (req.url === "/wake") {
      if (body && body.repo === "octo/fail-wake") return send(500, { error: "boom" });
      return send(202, { running: true });
    }
    if (req.url === "/stop") return send(200, { running: false });
    return send(404, { error: "not found" });
  });
});
await new Promise((r) => ct.listen(0, "127.0.0.1", r));
const CT_ORIGIN = `http://127.0.0.1:${ct.address().port}`;

let ipSeq = 0;
const ip = () => ({ "cf-connecting-ip": `198.51.100.${150 + (ipSeq++ % 60)}` });

// One request; reads the body once. Writes carry the bridge's own origin unless
// the test sets another one. Returns { status, text, json() }.
async function call(base, path, { method = "GET", cookie = null, body, origin, headers = {} } = {}) {
  const h = { ...headers };
  if (cookie) h.cookie = cookie;
  if (body !== undefined) h["content-type"] = "application/json";
  if (method !== "GET") h.origin = origin ?? base;
  const res = await fetch(base + path, {
    method, headers: h, redirect: "manual",
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  browserSeen.push(text);
  return { status: res.status, text, json: () => JSON.parse(text) };
}

// Signs in and returns the owner cookie pair (name=value).
async function ownerCookie(base) {
  const res = await fetch(base + "/auth", {
    method: "POST", headers: { "content-type": "application/json", ...ip() }, body: JSON.stringify({ token: OWNER }),
  });
  await res.text();
  return (res.headers.get("set-cookie") || "").split(";")[0];
}

const hostAuth = { authorization: `Bearer ${HOST}` };
let A, B, C;
try {
  A = await startDev("a", {
    OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, VAULT_KEY: VAULT, WAKE_TOKEN: WAKE,
    MODRON_TEST_HOOKS: "1", GITHUB_API_ORIGIN: GH_ORIGIN, CONTAINER_URL: CT_ORIGIN,
  });
  B = await startDev("b", {
    OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, VAULT_KEY: VAULT, WAKE_TOKEN: WAKE,
    MODRON_TEST_HOOKS: "1", GITHUB_API_ORIGIN: GH_ORIGIN, CONTAINER_URL: "http://192.0.2.1:9",
  });
  C = await startDev("c", {
    OWNER_TOKEN: OWNER, HOST_TOKEN: HOST, WAKE_TOKEN: WAKE,
    MODRON_TEST_HOOKS: "1", GITHUB_API_ORIGIN: GH_ORIGIN, CONTAINER_URL: CT_ORIGIN,
  });

  // ---- A: the full path ----
  const cookie = await ownerCookie(A.base);
  check("owner signs in on A", cookie.startsWith("modron_owner="), cookie.split("=")[0]);

  const noCookie = await call(A.base, "/api/github/accounts");
  check("no cookie: account list is 401", noCookie.status === 401, String(noCookie.status));
  const noCookieRuns = await call(A.base, "/api/runs");
  check("no cookie: run list is 401", noCookieRuns.status === 401, String(noCookieRuns.status));

  const crossOrigin = await call(A.base, "/api/github/accounts", {
    method: "POST", cookie, body: { token: TOKEN_ALICE }, origin: "https://evil.example",
  });
  check("cross-origin account write is refused 403", crossOrigin.status === 403, String(crossOrigin.status));

  const added = await call(A.base, "/api/github/accounts", { method: "POST", cookie, body: { token: TOKEN_ALICE } });
  const alice = added.json();
  check("adding a valid token answers 201 with the login", added.status === 201 && alice.login === "alice", added.text.slice(0, 120));
  check("the account row has an id, kind token, and no token", /^[0-9a-f]{32}$/.test(alice.id) && alice.kind === "token" && !added.text.includes(TOKEN_ALICE));

  const refused = await call(A.base, "/api/github/accounts", { method: "POST", cookie, body: { token: TOKEN_BAD } });
  const refusedBody = refused.json();
  check("a token GitHub refuses is 400 with GitHub's status", refused.status === 400 && refusedBody.github_status === 401, refused.text);
  check("the refusal body never echoes the token", !refused.text.includes(TOKEN_BAD));

  const bad = await call(A.base, "/api/github/accounts", { method: "POST", cookie, body: { token: "has space" } });
  check("a token with whitespace is 400 before GitHub is called", bad.status === 400, String(bad.status));

  const bob = (await call(A.base, "/api/github/accounts", { method: "POST", cookie, body: { token: TOKEN_BOB } })).json();
  check("a second account is added", bob.login === "bob");

  const list = await call(A.base, "/api/github/accounts", { cookie });
  const accounts = list.json().accounts;
  check("the account list has both logins and no token", accounts.length === 2 && !list.text.includes(TOKEN_ALICE) && !list.text.includes(TOKEN_BOB),
    list.text.slice(0, 200));
  check("each account row is id, login, kind only", accounts.every((a) => Object.keys(a).sort().join() === "id,kind,login"));

  // repos: paging, filter and shape
  const r1 = (await call(A.base, `/api/github/repos?account=${alice.id}`, { cookie })).json();
  check("repos page 1 is 50 with next_page 2", r1.repos.length === 50 && r1.next_page === 2, `${r1.repos.length} ${r1.next_page}`);
  check("a repo row carries exactly the 10.10 fields", Object.keys(r1.repos[0]).sort().join() === "default_branch,full_name,private,pushed_at",
    Object.keys(r1.repos[0]).join());
  const r5 = (await call(A.base, `/api/github/repos?account=${alice.id}&page=5`, { cookie })).json();
  check("repos page 5 holds the rest and no next_page", r5.repos.length === 30 && r5.next_page === null, `${r5.repos.length} ${r5.next_page}`);
  const q1 = (await call(A.base, `/api/github/repos?account=${alice.id}&q=needle`, { cookie })).json();
  check("q filters on the bridge: 50 per page, next_page 2", q1.repos.length === 50 && q1.next_page === 2 && q1.repos.every((r) => r.full_name.includes("needle")),
    `${q1.repos.length} ${q1.next_page}`);
  const q3 = (await call(A.base, `/api/github/repos?account=${alice.id}&q=NEEDLE&page=3`, { cookie })).json();
  check("q is case-insensitive over the scanned pages (115 matches, page 3 has 15)", q3.repos.length === 15 && q3.next_page === null,
    `${q3.repos.length} ${q3.next_page}`);
  const ghPaths = ghRequests.filter((r) => r.path === "/user/repos");
  check("the filter reads GitHub pages of 100, at most three", ghPaths.some((r) => r.query.includes("per_page=100"))
    && ghPaths.filter((r) => r.query.includes("per_page=100")).length === 3 * 2 /* two filtered requests, three pages each */,
    String(ghPaths.filter((r) => r.query.includes("per_page=100")).length));
  check("every GitHub call carries the modron-bridge user agent and the GitHub accept type",
    ghRequests.every((r) => r.ua === "modron-bridge" && r.accept === "application/vnd.github+json"));
  const badAccount = await call(A.base, `/api/github/repos?account=zz`, { cookie });
  check("a malformed account id is 400", badAccount.status === 400, String(badAccount.status));
  const missingAccount = await call(A.base, `/api/github/repos?account=${"0".repeat(32)}`, { cookie });
  check("an unknown account id is 404", missingAccount.status === 404, String(missingAccount.status));
  const badPage = await call(A.base, `/api/github/repos?account=${alice.id}&page=0`, { cookie });
  check("page 0 is 400", badPage.status === 400, String(badPage.status));

  // invites: viewer and approver are refused on every cloud route
  const sessionId = randomUUID();
  const viewerKey = "cloud-" + randomBytes(8).toString("hex");
  const reg = await call(A.base, `/api/host/sessions/${sessionId}`, {
    method: "PUT", headers: hostAuth,
    body: { title: "cloud test", host_label: "test", host_kind: "local", viewer_key: viewerKey, state: "idle" },
  });
  check("host registers a session for invites", reg.status === 204, String(reg.status));
  for (const role of ["viewer", "approver"]) {
    const inv = (await call(A.base, "/api/invites", { method: "POST", cookie, body: { session_id: sessionId, role, ttl_hours: 1 } })).json();
    const token = new URL(inv.url).pathname.slice("/i/".length);
    const q = `invite=${encodeURIComponent(token)}`;
    const acct = await call(A.base, `/api/github/accounts?${q}`);
    const repos = await call(A.base, `/api/github/repos?account=${alice.id}&${q}`);
    const runs = await call(A.base, `/api/runs?${q}`);
    const runPost = await call(A.base, `/api/runs?${q}`, {
      method: "POST", body: { account: alice.id, repo: "octo/app" },
    });
    check(`${role} invite: account list is 403`, acct.status === 403, String(acct.status));
    check(`${role} invite: repo list is 403`, repos.status === 403, String(repos.status));
    check(`${role} invite: run list is 403`, runs.status === 403, String(runs.status));
    check(`${role} invite: start run is 403 and starts nothing`, runPost.status === 403 && ctRequests.every((r) => r.body?.repo !== "octo/app"),
      String(runPost.status));
  }

  // runs: start, the container call, the registration link, stop, wake
  const beforeWake = ctRequests.length;
  const started = await call(A.base, "/api/runs", {
    method: "POST", cookie,
    body: { account: alice.id, repo: "octo/app", branch: "main", handoff_ref: "modron/handoff/abc" },
  });
  const run = started.json();
  check("start run answers 201 in state starting", started.status === 201 && run.state === "starting" && run.branch === "main" && run.session_url === null,
    started.text.slice(0, 200));
  check("the run row has the 10.10 fields", ["id", "repo", "account", "branch", "state", "reason", "created_at", "session_url"].every((k) => k in run), Object.keys(run).join());
  check("the run row never carries the git token", !started.text.includes(TOKEN_ALICE));
  const wakeCall = ctRequests[beforeWake];
  check("the container gets /wake with the WAKE bearer", wakeCall && wakeCall.path === "/wake" && wakeCall.auth === `Bearer ${WAKE}`, JSON.stringify(wakeCall?.auth));
  check("the wake body carries the per-run environment",
    wakeCall && wakeCall.body.run_id === run.id && wakeCall.body.bridge_url === `ws://127.0.0.1:${A.base.split(":")[2]}` &&
      wakeCall.body.host_token === HOST && wakeCall.body.repo === "octo/app" && wakeCall.body.branch === "main" &&
      wakeCall.body.git_token === TOKEN_ALICE && wakeCall.body.handoff_ref === "modron/handoff/abc",
    JSON.stringify(wakeCall?.body && { ...wakeCall.body, git_token: "<set>", host_token: "<set>" }));

  const runList = (await call(A.base, "/api/runs", { cookie })).json();
  check("the run list shows the run starting", runList.runs.some((r) => r.id === run.id && r.state === "starting"));

  for (const [label, body, want] of [
    ["a repo without a slash", { account: alice.id, repo: "octo" }, 400],
    ["a branch that starts with a dash", { account: alice.id, repo: "octo/app", branch: "-x" }, 400],
    ["a malformed account", { account: "zz", repo: "octo/app" }, 400],
    ["an unknown account", { account: "0".repeat(32), repo: "octo/app" }, 404],
  ]) {
    const res = await call(A.base, "/api/runs", { method: "POST", cookie, body });
    check(`start run refuses ${label}`, res.status === want, String(res.status));
  }

  // The host registers the run's session: the run turns running and links it.
  const runSession = randomUUID();
  const runKey = "run-" + randomBytes(8).toString("hex");
  const regRun = await call(A.base, `/api/host/sessions/${runSession}`, {
    method: "PUT", headers: hostAuth,
    body: { title: "cloud run", host_label: "container", host_kind: "cloud", viewer_key: runKey, state: "running", run_id: run.id },
  });
  check("a registration carrying run_id answers 204", regRun.status === 204, String(regRun.status));
  const linked = (await call(A.base, "/api/runs", { cookie })).json().runs.find((r) => r.id === run.id);
  const openUrl = (await call(A.base, "/api/sessions", { cookie })).json().find((s) => s.viewer_key === runKey)?.open_url;
  check("the run is running and its session_url is the portal open URL", linked?.state === "running" && linked.session_url === openUrl && openUrl !== undefined,
    JSON.stringify(linked));
  const badRunId = await call(A.base, `/api/host/sessions/${runSession}`, {
    method: "PUT", headers: hostAuth,
    body: { title: "cloud run", host_label: "container", host_kind: "cloud", viewer_key: runKey, state: "running", run_id: "Bad Id" },
  });
  check("a malformed run_id on registration is 400", badRunId.status === 400, String(badRunId.status));

  const stop = await call(A.base, `/api/runs/${run.id}/stop`, { method: "POST", cookie });
  check("stop answers 200 stopped", stop.status === 200 && stop.json().state === "stopped", stop.text.slice(0, 160));
  const stopCall = ctRequests.at(-1);
  check("stop reaches the container /stop with the run id and the bearer",
    stopCall && stopCall.path === "/stop" && stopCall.auth === `Bearer ${WAKE}` && stopCall.body.run_id === run.id);

  const wake = await call(A.base, `/api/runs/${run.id}/wake`, { method: "POST", cookie });
  check("wake answers 200 starting again", wake.status === 200 && wake.json().state === "starting", wake.text.slice(0, 160));
  const rewake = ctRequests.at(-1);
  check("a wake after a stop sends the same git token and handoff ref",
    rewake && rewake.path === "/wake" && rewake.body.git_token === TOKEN_ALICE && rewake.body.handoff_ref === "modron/handoff/abc");

  const unknownRun = await call(A.base, `/api/runs/${randomUUID()}/stop`, { method: "POST", cookie });
  check("stop of an unknown run is 404", unknownRun.status === 404, String(unknownRun.status));

  const failed = await call(A.base, "/api/runs", {
    method: "POST", cookie, body: { account: alice.id, repo: "octo/fail-wake" },
  });
  const failedRow = failed.json();
  check("a wake the container refuses sets failed with a short status-only reason",
    failed.status === 201 && failedRow.state === "failed" && failedRow.reason === "wake failed: container answered status 500",
    failed.text.slice(0, 200));

  // The account limit: 10 accounts at most.
  for (let i = 0; i < 8; i++) await call(A.base, "/api/github/accounts", { method: "POST", cookie, body: { token: TOKEN_ALICE } });
  const eleventh = await call(A.base, "/api/github/accounts", { method: "POST", cookie, body: { token: TOKEN_ALICE } });
  check("an eleventh account is 409", eleventh.status === 409, String(eleventh.status));

  const delBob = await call(A.base, `/api/github/accounts/${bob.id}`, { method: "DELETE", cookie, origin: A.base });
  check("removing an account answers 204", delBob.status === 204, String(delBob.status));
  const delBob2 = await call(A.base, `/api/github/accounts/${bob.id}`, { method: "DELETE", cookie, origin: A.base });
  check("removing it again is 404", delBob2.status === 404, String(delBob2.status));

  check("no browser-facing response carries a git token",
    !browserSeen.some((t) => t.includes(TOKEN_ALICE) || t.includes(TOKEN_BOB)));
  check("the fake container saw only /wake and /stop calls",
    ctRequests.every((r) => r.path === "/wake" || r.path === "/stop"));

  // ---- B: VAULT_KEY set, container URL not https ----
  const bCookie = await ownerCookie(B.base);
  const bAdd = await call(B.base, "/api/github/accounts", { method: "POST", cookie: bCookie, body: { token: TOKEN_ALICE } });
  check("on B (vault on) an account is added", bAdd.status === 201, String(bAdd.status));
  const bRun = await call(B.base, "/api/runs", { method: "POST", cookie: bCookie, body: { account: bAdd.json().id, repo: "octo/app" } });
  check("on B runs answer 503 cloud runs are not configured (container URL not https)",
    bRun.status === 503 && bRun.text === "cloud runs are not configured", bRun.text);
  const bList = await call(B.base, "/api/runs", { cookie: bCookie });
  check("the run list still answers on B", bList.status === 200 && bList.json().runs.length === 0, String(bList.status));

  // ---- C: no VAULT_KEY ----
  const cCookie = await ownerCookie(C.base);
  const cAccounts = await call(C.base, "/api/github/accounts", { cookie: cCookie });
  check("without VAULT_KEY the account list answers 503 even with a cookie", cAccounts.status === 503, String(cAccounts.status));
  const cRuns = await call(C.base, "/api/runs", { method: "POST", cookie: cCookie, body: { account: "0".repeat(32), repo: "octo/app" } });
  check("without VAULT_KEY a run start answers 503", cRuns.status === 503, String(cRuns.status));
  const cMe = await call(C.base, "/api/me", { cookie: cCookie });
  check("the rest of the portal still answers without VAULT_KEY", cMe.status === 200, String(cMe.status));
} catch (e) {
  console.log("FAIL harness -- " + (e && e.stack ? e.stack : e));
  failures.push("harness");
} finally {
  for (const c of children) { try { process.kill(-c.pid, "SIGTERM"); } catch { /* gone */ } }
  gh.close();
  ct.close();
  await sleep(500);
  rmSync(stateRoot, { recursive: true, force: true });
  if (madePlaceholder) rmSync(appDir, { recursive: true, force: true });
}
console.log(failures.length === 0 ? "ALL PASS" : `${failures.length} FAILED: ${failures.join(", ")}`);
process.exit(failures.length === 0 ? 0 : 1);
