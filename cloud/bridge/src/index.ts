// modron remote-control bridge - a transport-only relay.
// One Durable Object instance per share key. Agent connects to
// /agent/<key>; viewers open /s/<key> then /ws/<key>.
// Pages: POST /pages/<key> with `Authorization: Bearer <PUBLISH_TOKEN>`
// stores, GET /p/<id> renders. PUBLISH_TOKEN is a deploy-time Wrangler
// secret; the router checks it BEFORE any Durable Object is created, so
// an unauthenticated request never reaches storage. The POST is then
// dispatched to the DO named by <key> (the key only selects storage; it
// is not a credential). Page bodies live in the session DO's own
// storage; only the validated publish path writes id->key entries into
// the dedicated __pages__ registry DO (via RPC, which has no externally
// routable surface).
// Trust model: the share key is the credential for watching a session
// (treat the URL as the secret). Publishing pages additionally requires
// the PUBLISH_TOKEN secret; the share key alone cannot publish.

const MAX_BUFFER = 200;
const MAX_FRAME = 96 * 1024; // per-frame cap (DO storage values cap at 128 KiB)
// Agent frames above MAX_FRAME are still relayed live (never stored) up to
// this size, so a long session's attach snapshot reaches its viewers.
const MAX_FORWARD = 4 * 1024 * 1024;

// A bare keepalive reply from the agent ({"type":"pong", ...}).
function isKeepalive(message: string): boolean {
  if (message.length > 256) return false;
  try {
    const parsed = JSON.parse(message) as { type?: unknown };
    return parsed !== null && typeof parsed === "object" && parsed.type === "pong";
  } catch {
    return false;
  }
}

// Viewers and the agent use DIFFERENT keys for the same session. The agent
// connects with its secret key; the share link carries only the viewer key,
// a one-way hash of it. Both resolve to the same Durable Object (named by
// the viewer key), but someone holding a share link cannot derive the agent
// key and so cannot connect as the agent and impersonate it to other
// viewers. Must match `viewer_key` in crates/modron-server/src/rc.rs.
async function viewerKeyOf(agentKey: string): Promise<string> {
  const bytes = new TextEncoder().encode("modron-viewer:" + agentKey);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  let hex = "";
  for (let i = 0; i < 16; i++) hex += digest[i].toString(16).padStart(2, "0");
  return hex;
}
// Durable Object storage rejects values above 128 KiB - caps stay under
// it (cycle-3 F4: the 256KB/1MB caps passed validation then blew up at
// storage.put with an opaque 500 / silently-missing replay frame).
const PAGE_MAX_BYTES = 96 * 1024; // /pages body cap (DO storage values cap at 128 KiB)
const PAGE_TTL_MS = 7 * 24 * 3600 * 1000; // pages GC 7 days after publish
const BUFFER_IDLE_MS = 6 * 3600 * 1000; // buffer GC 6h after both sides leave
const REGISTRY_DO = "__pages__"; // DO name holding the page-id -> key registry
const KEY_RE = /^[a-z0-9-]{1,64}$/; // share keys are 8 lowercase hex chars; stay permissive
const ID_RE = /^[0-9a-f-]{1,64}$/i; // page ids are crypto.randomUUID()

// Sequence numbers are zero-padded so lexical order of the per-frame
// storage keys equals numeric order (replay + trim rely on this).
const frameKey = (seq: number) => "frame:" + String(seq).padStart(12, "0");

interface Env {
  BRIDGE: DurableObjectNamespace<SessionBridge>;
  // Wrangler secret (`npx wrangler secret put PUBLISH_TOKEN`). Unset means
  // publishing is disabled (503). Never put its value in wrangler.jsonc.
  PUBLISH_TOKEN?: string;
}

// Constant-time string equality for the publish token. Compares UTF-8
// bytes and always walks the longer input, so the timing does not reveal
// the position of the first mismatch.
function constantTimeEquals(a: string, b: string): boolean {
  const enc = new TextEncoder();
  const x = enc.encode(a);
  const y = enc.encode(b);
  let diff = x.length ^ y.length;
  const n = Math.max(x.length, y.length);
  for (let i = 0; i < n; i++) diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return diff === 0;
}

// Top-level publish gate, run by the router before any DO is touched.
// Returns a Response to send back, or null when the request may proceed.
function publishGate(request: Request, env: Env): Response | null {
  if (!env.PUBLISH_TOKEN) {
    return new Response("publishing is disabled: this bridge has no PUBLISH_TOKEN secret", { status: 503 });
  }
  const auth = request.headers.get("authorization") ?? "";
  const presented = auth.startsWith("Bearer ") ? auth.slice("Bearer ".length) : "";
  if (!constantTimeEquals(presented, env.PUBLISH_TOKEN)) {
    return new Response("unauthorized", { status: 401 });
  }
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    const len = Number(declared);
    if (!Number.isInteger(len) || len < 0) return new Response("bad content-length", { status: 400 });
    if (len > PAGE_MAX_BYTES) return new Response("page too large (max 96KB)", { status: 413 });
  }
  return null;
}

interface StoredFrame {
  text: string;
  ts: number;
}
interface StoredPage {
  title: string;
  content_md: string;
  ts: number;
}
interface RegistryEntry {
  key: string;
  ts: number;
}

import { DurableObject } from "cloudflare:workers";

export class SessionBridge extends DurableObject {
  state: DurableObjectState;
  env: Env;
  // Serializes agent-frame appends within one DO incarnation so two
  // concurrent webSocketMessage events can never interleave the
  // read-increment-write of the sequence counter (input gates cover
  // cross-incarnation delivery; this covers same-incarnation calls).
  appendChain: Promise<void> = Promise.resolve();

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.state = state;
    this.env = env;
  }
  keyOf(ws: WebSocket) {
    for (const tag of this.state.getTags(ws)) { if (tag.startsWith("key:")) return tag.slice(4); }
    return "";
  }
  roleOf(ws: WebSocket) {
    for (const tag of this.state.getTags(ws)) { if (tag.startsWith("role:")) return tag.slice(5); }
    return "";
  }
  viewers() { return this.state.getWebSockets("role:viewer"); }
  agentSocket() { return this.state.getWebSockets("role:agent").at(-1) ?? null; }

  // --- alarms: one handler serves all three DO roles (session buffers,
  // page storage in per-key DOs, and the __pages__ registry), because it
  // only touches whatever keys actually exist in this instance. ---
  async ensureAlarmAt(deadline: number) {
    const cur = await this.state.storage.getAlarm();
    if (cur === null || cur > deadline) await this.state.storage.setAlarm(deadline);
  }

  async alarm() {
    const now = Date.now();
    let next: number | null = null;

    // 1) Expired pages (page:<id> in per-key DOs).
    const pages = await this.state.storage.list<StoredPage>({ prefix: "page:" });
    const registry = this.env.BRIDGE.get(this.env.BRIDGE.idFromName(REGISTRY_DO));
    for (const [k, page] of pages) {
      const expiry = page.ts + PAGE_TTL_MS;
      if (expiry <= now) {
        await this.state.storage.delete(k);
        // Best-effort registry cleanup; the registry's own alarm sweep
        // below/over there removes anything this misses.
        try { await registry.deletePageEntry(k.slice(5)); } catch { /* swept later */ }
      } else {
        next = next === null ? expiry : Math.min(next, expiry);
      }
    }

    // 2) Expired registry entries (reg:<id>, only in the __pages__ DO).
    const regs = await this.state.storage.list<RegistryEntry>({ prefix: "reg:" });
    for (const [k, entry] of regs) {
      const expiry = entry.ts + PAGE_TTL_MS;
      if (expiry <= now) await this.state.storage.delete(k);
      else next = next === null ? expiry : Math.min(next, expiry);
    }

    // 3) Idle session buffer: delete frames + counters once BOTH the
    // agent and every viewer have been gone for BUFFER_IDLE_MS.
    // ("agent-buffer" is the pre-per-frame-keys layout; sweep it so
    // upgrades don't leave stale buffers behind.)
    const seq = await this.state.storage.get<number>("seq");
    if (seq !== undefined) {
      const seenAgent = (await this.state.storage.get<number>("seen:agent")) ?? 0;
      const seenViewer = (await this.state.storage.get<number>("seen:viewer")) ?? 0;
      const idleSince = Math.max(seenAgent, seenViewer) + BUFFER_IDLE_MS;
      const empty =
        this.state.getWebSockets("role:agent").length === 0 &&
        this.state.getWebSockets("role:viewer").length === 0;
      if (empty && idleSince <= now) {
        const frameKeys = [...(await this.state.storage.list({ prefix: "frame:" })).keys()];
        await this.state.storage.delete([
          ...frameKeys, "seq", "base", "seen:agent", "seen:viewer", "agent-buffer",
        ]);
      } else {
        next = next === null ? idleSince : Math.min(next, idleSince);
      }
    }

    if (next !== null) await this.state.storage.setAlarm(next);
  }

  // --- pages ---

  // POST /pages/<key>. Reached only through the router, which has already
  // checked PUBLISH_TOKEN (publishGate) before dispatching to this DO.
  // The DO itself only validates the path key and the body size.
  async handlePagePublish(request: Request, key: string): Promise<Response> {
    if (!KEY_RE.test(key)) return new Response("invalid key", { status: 400 });
    const raw = await request.text();
    // Measure bytes, not UTF-16 code units: a multi-byte page can be
    // under the limit by length and over it on the wire.
    if (new TextEncoder().encode(raw).length > PAGE_MAX_BYTES) {
      return new Response("page too large (max 96KB)", { status: 413 });
    }
    let body: { title?: string; content_md?: string };
    try { body = JSON.parse(raw) as { title?: string; content_md?: string }; }
    catch { return new Response("bad json", { status: 400 }); }
    const id = crypto.randomUUID();
    const ts = Date.now();
    await this.state.storage.put<StoredPage>("page:" + id, {
      title: body.title || "modron page",
      content_md: body.content_md || "",
      ts,
    });
    // Only the validated path reaches here, and the registry write is an
    // RPC call (no HTTP route can invoke it).
    await this.env.BRIDGE.get(this.env.BRIDGE.idFromName(REGISTRY_DO))
      .publishPageEntry(id, key, ts);
    await this.ensureAlarmAt(ts + PAGE_TTL_MS);
    const origin = new URL(request.url).origin;
    return new Response(JSON.stringify({ id, url: origin + "/p/" + id }), { headers: { "content-type": "application/json" } });
  }

  // GET /p/<id> - runs in the __pages__ registry DO (the only place the
  // router sends /p/*). Registry lookup, then fetch the body from the
  // session DO that owns it.
  async handlePageView(request: Request, id: string): Promise<Response> {
    const notFound = () => new Response("not found", {
      status: 404,
      headers: { "x-content-type-options": "nosniff" },
    });
    if (!ID_RE.test(id)) return notFound();
    const entry = await this.state.storage.get<RegistryEntry>("reg:" + id);
    if (!entry) return notFound();
    const page = await this.env.BRIDGE.get(this.env.BRIDGE.idFromName(entry.key))
      .getPage(id);
    if (!page) return notFound();
    const esc = (t: string) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    const paras = page.content_md.split(/\n\n/).map((p) => "<p>" + esc(p).replace(/\n/g, "<br>") + "</p>").join("");
    const html = "<!doctype html><html><head><meta charset=utf-8><title>" + esc(page.title) + "</title></head><body><h1>" + esc(page.title) + "</h1>" + paras + "</body></html>";
    return new Response(html, {
      headers: {
        "content-type": "text/html; charset=utf-8",
        "content-security-policy": "default-src 'none'",
        "x-content-type-options": "nosniff",
      },
    });
  }

  // --- viewer page (GET /s/<key>) ---

  handleViewerPage(request: Request, key: string): Response {
    // Per-request nonce keeps the inline bootstrap script CSP-clean
    // without loosening default-src 'none'.
    const nonce = crypto.randomUUID().replace(/-/g, "");
    const html = `<!doctype html>
<html>
<head>
<meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>modron ${key}</title>
<style>
  body { background: #0d1117; color: #e6edf3; font: 14px/1.5 ui-monospace, monospace; margin: 0; padding: 1rem; }
  .agent { white-space: pre-wrap; }
  .reasoning { white-space: pre-wrap; opacity: 0.55; font-style: italic; }
  .note { color: #7d8590; margin: 0.5rem 0; }
  .dim { color: #7d8590; white-space: pre-wrap; }
</style>
</head>
<body>
<div id="log"></div>
<script nonce="${nonce}">
(function () {
  var log = document.getElementById("log");
  var key = location.pathname.split("/")[2] || "";
  var proto = location.protocol === "https:" ? "wss:" : "ws:";
  var current = null;
  function line(cls) {
    current = document.createElement("div");
    if (cls) current.className = cls;
    log.appendChild(current);
    return current;
  }
  function scroll() { window.scrollTo(0, document.body.scrollHeight); }
  var ws = new WebSocket(proto + "//" + location.host + "/ws/" + key);
  ws.onopen = function () { ws.send(JSON.stringify({ type: "sync" })); };
  ws.onmessage = function (e) {
    var f = null;
    try { f = JSON.parse(e.data); } catch (err) { /* raw text */ }
    if (!f || typeof f !== "object") { line("agent").textContent = String(e.data); scroll(); return; }
    if (f.type === "delta") {
      var d = f.delta || {};
      if (d.kind === "text") {
        if (!current || current.className !== "agent") line("agent");
        current.textContent += d.text || "";
      } else if (d.kind === "reasoning") {
        if (!current || current.className !== "reasoning") line("reasoning");
        current.textContent += d.text || "";
      } else if (d.kind === "done") {
        current = null;
      }
    } else if (f.type === "note") {
      line("note").textContent = "\\u00b7 " + (f.text || "");
      current = null;
    } else if (f.type === "snapshot") {
      line("note").textContent = "\\u00b7 transcript snapshot (" + ((f.events || []).length) + " events)";
      current = null;
    } else {
      line("dim").textContent = "\\u00b7 " + (f.type || String(e.data));
      current = null;
    }
    scroll();
  };
  ws.onclose = function () { line("note").textContent = "\\u00b7 disconnected - reload to reconnect"; };
})();
</script>
</body>
</html>`;
    return new Response(html, {
      headers: {
        "content-type": "text/html; charset=utf-8",
        "content-security-policy":
          "default-src 'none'; script-src 'nonce-" + nonce + "'; style-src 'unsafe-inline'; connect-src 'self'",
        "x-content-type-options": "nosniff",
      },
    });
  }

  // --- transport ---

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    let m: RegExpMatchArray | null;
    if ((m = path.match(/^\/pages\/([^/]+)$/)) && request.method === "POST") {
      return this.handlePagePublish(request, m[1]);
    }
    if ((m = path.match(/^\/p\/([^/]+)$/)) && request.method === "GET") {
      return this.handlePageView(request, m[1]);
    }
    if ((m = path.match(/^\/s\/([^/]+)$/)) && request.method === "GET") {
      return this.handleViewerPage(request, m[1]);
    }
    if ((m = path.match(/^\/(?:agent|ws)\/([^/]+)$/)) && request.headers.get("upgrade") === "websocket") {
      const key = m[1];
      const role = path.startsWith("/agent/") ? "agent" : "viewer";
      const now = Date.now();
      await this.state.storage.put("seen:" + role, now);
      await this.ensureAlarmAt(now + BUFFER_IDLE_MS);
      const pair = new WebSocketPair();
      // The tag only marks the socket as keyed; the agent's secret key is
      // never stored.
      this.state.acceptWebSocket(pair[1], ["role:" + role, "key:" + (role === "agent" ? "agent" : key)]);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    return new Response("modron bridge - see cloud/bridge/README.md", { status: 404 });
  }

  // Agent frames land in per-frame storage keys (no whole-buffer
  // rewrite); viewers fan out verbatim.
  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    const key = this.keyOf(ws);
    if (!key) return;
    const role = this.roleOf(ws);
    if (typeof message !== "string") {
      try { ws.send(JSON.stringify({ type: "note", text: "binary frames are not supported - dropped" })); } catch { /* gone */ }
      return;
    }
    // Viewer frames are small commands; agent frames can be large (an
    // attach snapshot carries the whole transcript), so they are forwarded
    // live up to MAX_FORWARD and only STORED for replay when they fit a
    // Durable Object value (MAX_FRAME).
    const limit = role === "agent" ? MAX_FORWARD : MAX_FRAME;
    if (message.length > limit) {
      const text = role === "agent"
        ? "frame too large (4MB limit) - dropped"
        : "frame too large (96KB limit) - dropped";
      try { ws.send(JSON.stringify({ type: "note", text })); } catch { /* gone */ }
      return;
    }
    if (role !== "agent") {
      // Viewer: {"type":"sync"} replays the buffer; anything else is
      // forwarded to the agent verbatim.
      let parsed: { type?: unknown } | null = null;
      try { parsed = JSON.parse(message) as { type?: unknown }; } catch { /* not json */ }
      if (parsed !== null && typeof parsed === "object" && parsed.type === "sync") {
        const frames = await this.state.storage.list<StoredFrame>({ prefix: "frame:", limit: MAX_BUFFER });
        for (const frame of frames.values()) { try { ws.send(frame.text); } catch { break; } }
        return;
      }
      const now = Date.now();
      await this.state.storage.put("seen:viewer", now);
      const agent = this.agentSocket();
      try { agent?.send(message); } catch { /* gone */ }
      return;
    }
    // Keepalive pongs are not history: storing one every 30 s would evict
    // the whole replay buffer after about 100 idle minutes.
    if (isKeepalive(message)) {
      await this.state.storage.put("seen:agent", Date.now());
      return;
    }
    if (message.length <= MAX_FRAME) {
      await this.appendAgentFrame(message).catch(() => { /* storage hiccup - still fan out live */ });
    }
    for (const viewer of this.viewers()) { try { viewer.send(message); } catch { /* gone */ } }
  }

  appendAgentFrame(text: string): Promise<void> {
    const run = this.appendChain.then(async () => {
      const now = Date.now();
      const seq = ((await this.state.storage.get<number>("seq")) ?? 0) + 1;
      const base = (await this.state.storage.get<number>("base")) ?? 1;
      const updates: Record<string, unknown> = {
        "seq": seq,
        [frameKey(seq)]: { text, ts: now } satisfies StoredFrame,
        "seen:agent": now,
      };
      // Trim: once over MAX_BUFFER frames, advance the low-water mark and
      // delete the oldest keys (<= 1 deletion per frame in steady state).
      const newBase = seq - MAX_BUFFER + 1;
      if (newBase > base) {
        updates["base"] = newBase;
        const dead: string[] = [];
        for (let s = base; s < newBase; s++) dead.push(frameKey(s));
        await this.state.storage.delete(dead);
      }
      await this.state.storage.put(updates);
    });
    this.appendChain = run.catch(() => { /* keep the chain alive */ });
    return run;
  }

  async webSocketClose(ws: WebSocket) {
    const role = this.roleOf(ws);
    await this.state.storage.put("seen:" + role, Date.now());
    await this.ensureAlarmAt(Date.now() + BUFFER_IDLE_MS);
    if (role === "agent") {
      for (const viewer of this.viewers()) { try { viewer.send(JSON.stringify({ type: "note", text: "agent tunnel closed - run /remote-control again" })); } catch { /* gone */ } }
    }
  }

  // --- internal page RPCs (no HTTP route can reach these) ---

  // Called on the __pages__ registry DO by a validated publish path only.
  async publishPageEntry(id: string, key: string, ts: number): Promise<void> {
    await this.state.storage.put<RegistryEntry>("reg:" + id, { key, ts });
    await this.ensureAlarmAt(ts + PAGE_TTL_MS);
  }
  async deletePageEntry(id: string): Promise<void> {
    await this.state.storage.delete("reg:" + id);
  }
  async getPage(id: string): Promise<StoredPage | null> {
    return (await this.state.storage.get<StoredPage>("page:" + id)) ?? null;
  }
}

const router: ExportedHandler<Env> = {
  async fetch(request: Request, env: Env): Promise<Response> {
    const path = new URL(request.url).pathname;
    const validKey = (k: string) => KEY_RE.test(k);
    let m: RegExpMatchArray | null;
    // Page publish: the PUBLISH_TOKEN gate runs here, before any DO is
    // created or any storage is touched. Only then is the request sent
    // to the DO named by the path key (which selects storage, not auth).
    if ((m = path.match(/^\/pages\/([^/]+)$/)) && request.method === "POST") {
      const denied = publishGate(request, env);
      if (denied) return denied;
      if (!validKey(m[1])) return new Response("invalid key", { status: 400 });
      return env.BRIDGE.get(env.BRIDGE.idFromName(m[1])).fetch(request);
    }
    // Page views go to the registry DO, which maps id -> key and proxies
    // to the owning session DO.
    if ((m = path.match(/^\/p\/([^/]+)$/)) && request.method === "GET") {
      return env.BRIDGE.get(env.BRIDGE.idFromName(REGISTRY_DO)).fetch(request);
    }
    if (path === "/") return new Response("modron remote-control bridge - open a /s/<key> share link from modron");
    // The agent tunnel: /agent/<agent key>, routed to the DO named by the
    // derived viewer key (see viewerKeyOf).
    if ((m = path.match(/^\/agent\/([^/]+)$/))) {
      if (!validKey(m[1])) return new Response("invalid key", { status: 400 });
      const viewerKey = await viewerKeyOf(m[1]);
      return env.BRIDGE.get(env.BRIDGE.idFromName(viewerKey)).fetch(request);
    }
    // Viewers: /s/<viewer key> (page) and /ws/<viewer key> (socket).
    if ((m = path.match(/^\/s\/([^/]+)$/)) || (m = path.match(/^\/ws\/([^/]+)$/))) {
      if (!validKey(m[1])) return new Response("invalid key", { status: 400 });
      return env.BRIDGE.get(env.BRIDGE.idFromName(m[1])).fetch(request);
    }
    return new Response("modron bridge - see cloud/bridge/README.md", { status: 404 });
  },
};

export default router;
