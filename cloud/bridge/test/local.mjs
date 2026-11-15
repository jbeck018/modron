// Local end-to-end check of the bridge Worker under `wrangler dev --local`
// (miniflare / workerd on this machine; no Cloudflare account is touched).
// Run from cloud/bridge:  node test/local.mjs
import { spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const bridgeDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const appDir = join(bridgeDir, "public", "app");
const failures = [];
const check = (name, ok, detail = "") => {
  console.log((ok ? "PASS " : "FAIL ") + name + (ok || !detail ? "" : " -- " + detail));
  if (!ok) failures.push(name);
};

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

const port = await freePort();
const base = `http://127.0.0.1:${port}`;
const log = [];
const dev = spawn("npx", [
  "wrangler", "dev", "--local", "--ip", "127.0.0.1", "--port", String(port),
  "--show-interactive-dev-session=false",
], { cwd: bridgeDir, env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" } });
dev.stdout.on("data", (d) => log.push(String(d)));
dev.stderr.on("data", (d) => log.push(String(d)));

const cleanup = () => {
  dev.kill("SIGTERM");
  if (madePlaceholder) rmSync(appDir, { recursive: true, force: true });
};

async function waitReady(timeoutMs) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    try {
      const r = await fetch(base + "/", { redirect: "manual" });
      if (r.status === 200) return true;
    } catch { /* not listening yet */ }
    await new Promise((r) => setTimeout(r, 500));
  }
  return false;
}

try {
  if (!(await waitReady(90_000))) {
    throw new Error("wrangler dev did not become ready. Output:\n" + log.join(""));
  }
  console.log(`wrangler dev up on ${base}`);

  // Redirect from the share link to the app.
  const r1 = await fetch(base + "/s/abc", { redirect: "manual" });
  check("GET /s/abc is 302", r1.status === 302, "got " + r1.status);
  const want = `/app/?url=ws://127.0.0.1:${port}/ws/abc`;
  check("/s/abc redirects to the app with the ws url", r1.headers.get("location") === want,
    "location=" + r1.headers.get("location"));

  // Classic inline viewer still served.
  const r2 = await fetch(base + "/s/abc?classic=1");
  const t2 = await r2.text();
  check("/s/abc?classic=1 is the inline viewer", r2.status === 200 && t2.includes("<title>modron abc</title>") && (r2.headers.get("content-type") || "").startsWith("text/html"),
    "status " + r2.status);

  // App index from the bundle, revalidated every load.
  const r3 = await fetch(base + "/app/");
  const t3 = await r3.text();
  check("/app/ serves index.html", r3.status === 200 && t3.includes("<!doctype html") , "status " + r3.status);
  check("/app/ index is no-cache", r3.headers.get("cache-control") === "no-cache", "cache-control=" + r3.headers.get("cache-control"));

  // wasm MIME type and immutable cache for a hashed file.
  // The placeholder's wasm name, or the real bundle's first hashed .wasm.
  const wasm = readdirSync(appDir).find((f) => f.endsWith(".wasm"));
  const r4 = await fetch(base + "/app/" + wasm);
  await r4.arrayBuffer();
  check("/app/*.wasm is application/wasm", r4.status === 200 && r4.headers.get("content-type") === "application/wasm",
    "status " + r4.status + " type=" + r4.headers.get("content-type"));
  check("hashed asset is immutable", (r4.headers.get("cache-control") || "").includes("immutable"),
    "cache-control=" + r4.headers.get("cache-control"));

  // Unchanged routes.
  const r5 = await fetch(base + "/");
  check("/ unchanged", r5.status === 200 && (await r5.text()).startsWith("modron remote-control bridge"));
  const r6 = await fetch(base + "/nope");
  check("unknown path is 404 as before", r6.status === 404 && (await r6.text()).includes("see cloud/bridge/README.md"));

  // The viewer socket upgrades through the Worker to the Durable Object.
  const opened = await new Promise((resolve) => {
    const ws = new WebSocket(`ws://127.0.0.1:${port}/ws/abc`);
    const timer = setTimeout(() => { ws.close(); resolve(false); }, 10_000);
    ws.onopen = () => { clearTimeout(timer); ws.close(); resolve(true); };
    ws.onerror = () => { clearTimeout(timer); resolve(false); };
  });
  check("/ws/abc accepts a viewer socket", opened);

  // Protocol replay: an agent frame is buffered, and a web-app style viewer
  // (protocol hello, no sync) gets it on its first hello only.
  // A fresh agent key per run: the Durable Object buffer persists in
  // wrangler's local state, so a fixed key would accumulate frames.
  const agentKey = randomBytes(16).toString("hex");
  const viewerKey = createHash("sha256").update("modron-viewer:" + agentKey).digest("hex").slice(0, 32);
  const agentFrame = JSON.stringify({ type: "reply", id: 7, payload: { type: "welcome", protocol: 1 } });
  const agent = new WebSocket(`ws://127.0.0.1:${port}/agent/${agentKey}`);
  await new Promise((resolve, reject) => { agent.onopen = resolve; agent.onerror = reject; });
  agent.send(agentFrame);
  await new Promise((r) => setTimeout(r, 300));
  const received = [];
  const viewer = new WebSocket(`ws://127.0.0.1:${port}/ws/${viewerKey}`);
  viewer.onmessage = (e) => received.push(String(e.data));
  await new Promise((resolve, reject) => { viewer.onopen = resolve; viewer.onerror = reject; });
  const hello = JSON.stringify({ type: "hello", id: 0, payload: { client: "test", protocol: 1, token: null } });
  viewer.send(hello);
  await new Promise((r) => setTimeout(r, 800));
  check("viewer hello replays the buffered agent frame", received.length === 1 && received[0] === agentFrame,
    "received " + received.length + " message(s)");
  viewer.send(hello);
  await new Promise((r) => setTimeout(r, 800));
  check("a second hello does not replay again", received.length === 1, "received " + received.length + " message(s)");
  viewer.close();
  agent.close();
} catch (err) {
  check("test run", false, String(err && err.message ? err.message : err));
} finally {
  cleanup();
}

console.log(failures.length ? `${failures.length} check(s) failed` : "all checks passed");
process.exit(failures.length ? 1 : 0);
