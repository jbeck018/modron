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
const ID_RE = /^[0-9a-f-]{1,64}$/i; // page ids are crypto.randomUUID()

// True exactly once per viewer socket: the first call marks it and returns
// true, later calls return false. Survives DO hibernation via the socket
// attachment.
function claimFirstHello(ws: WebSocket): boolean {
  const att = ws.deserializeAttachment() as { helloSeen?: boolean } | null;
  if (att?.helloSeen === true) return false;
  ws.serializeAttachment({ helloSeen: true });
  return true;
}

// Sequence numbers are zero-padded so lexical order of the per-frame
// storage keys equals numeric order (replay + trim rely on this).
const frameKey = (seq: number) => "frame:" + String(seq).padStart(12, "0");

interface Env {
  BRIDGE: DurableObjectNamespace<SessionBridge>;
  // Wrangler secret (`npx wrangler secret put PUBLISH_TOKEN`). Unset means
  // publishing is disabled (503). Never put its value in wrangler.jsonc.
  PUBLISH_TOKEN?: string;
  // Workers Static Assets (wrangler.jsonc `assets`): the web app bundle
  // under public/app/. Requests reach the Worker first (run_worker_first).
  ASSETS: Fetcher;
  // Portal (cloud/bridge/README.md "Portal"). Wrangler secrets; unset means
  // the portal routes answer 503 and the share-link flow is unchanged.
  OWNER_TOKEN?: string;
  // Host registration and the agent tunnel. Unset leaves /agent open to
  // anyone holding the agent key, as before.
  HOST_TOKEN?: string;
  DIRECTORY: DurableObjectNamespace<Directory>;
  // Test-only (set by test/portal.mjs, never in a deployment): accepts a
  // ttl_seconds field on POST /api/invites so expiry can be tested quickly.
  MODRON_TEST_HOOKS?: string;
  // Web push (src/push.ts, README "Web push"). All three unset: /api/push/*
  // answers 503 and nothing else changes. PUSH_TEST_ORIGIN is honoured only
  // with MODRON_TEST_HOOKS (the e2e and test/push.mjs fake push service).
  VAPID_PUBLIC_KEY?: string;
  VAPID_PRIVATE_KEY?: string;
  VAPID_SUBJECT?: string;
  PUSH_TEST_ORIGIN?: string;
}

// constantTimeEquals (publish token, owner token, invite signatures) lives in
// ./portal so the Directory checks share it.

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
import { Directory } from "./directory";
import {
  PUSH_WORKER,
  importVapid,
  parseNotify,
  pushEndpointAllowed,
  sendPush,
  validSubscriptionKeys,
} from "./push";
import {
  KEY_RE,
  OWNER_COOKIE,
  TICKET_RE,
  UUID_RE,
  INVITE_ID_RE,
  constantTimeEquals,
  forbiddenFrame,
  inviteSignature,
  isLocalDev,
  judgeViewerFrame,
  stampRelayRole,
  ownerCookieHeader,
  parseHostSession,
  parseInvite,
  parseInviteToken,
  randomHex,
  readCookie,
  readJsonObject,
  sessionOpenUrl,
  sessionView,
  sha256Hex,
} from "./portal";

export { Directory };

// Router-to-object hand-off of a viewer's grant (owner | viewer | approver).
// Set only by the router on /ws/ upgrades; any client-sent copy is removed.
const GRANT_HEADER = "x-modron-grant";
const GRANTS = new Set(["owner", "viewer", "approver"]);

// --- web app (/app/*, static assets) ---

// Where GET /s/<viewer key> sends a browser: the unified web app, pointed at
// the viewer websocket on the same host. The scheme mirrors the request
// (https -> wss). The host comes from the parsed request URL and the key is
// already checked against KEY_RE, so neither can carry a character that
// would need escaping in the query string.
export function appRedirectLocation(requestUrl: URL, key: string): string {
  const wsScheme = requestUrl.protocol === "https:" ? "wss:" : "ws:";
  return "/app/?url=" + wsScheme + "//" + requestUrl.host + "/ws/" + key;
}

// Hashed bundle files (e.g. nsh.c30214878b.wasm) are immutable; anything
// else, including index.html, must be revalidated on every load so a new
// bundle is picked up at once.
export function appCacheControl(path: string): string {
  return /\.[0-9a-f]{8,}\.[A-Za-z0-9]+$/.test(path)
    ? "public, max-age=31536000, immutable"
    : "no-cache";
}

// Serve one file of the bundle from static assets, with the MIME type and
// cache policy the app needs. The Worker only sets headers; it never
// rewrites the body.
export async function serveApp(request: Request, env: Env): Promise<Response> {
  const path = new URL(request.url).pathname;
  const asset = await env.ASSETS.fetch(request);
  if (!asset.ok) return asset;
  const headers = new Headers(asset.headers);
  // Browsers only compile wasm via instantiateStreaming when it is served
  // as application/wasm; some asset paths otherwise come back as octet-stream.
  if (path.endsWith(".wasm")) headers.set("content-type", "application/wasm");
  headers.set("cache-control", appCacheControl(path));
  return new Response(asset.body, { status: asset.status, headers });
}

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
  // "owner" | "viewer" | "approver" for portal-granted viewers, "legacy" for
  // a share-key-only viewer (portal off). Agent sockets never carry one.
  grantOf(ws: WebSocket) {
    for (const tag of this.state.getTags(ws)) { if (tag.startsWith("grant:")) return tag.slice(6); }
    return "legacy";
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
      // The router sets the grant (ticket redeemed, or owner cookie checked).
      // With the portal on, a viewer socket without one is refused here too,
      // so this object fails closed even if the router is bypassed.
      const grant = role === "viewer" ? request.headers.get(GRANT_HEADER) : null;
      if (role === "viewer" && grant === null && this.env.OWNER_TOKEN) {
        return new Response("unauthorized", { status: 401 });
      }
      if (grant !== null && !GRANTS.has(grant)) return new Response("unauthorized", { status: 401 });
      const now = Date.now();
      await this.state.storage.put("seen:" + role, now);
      await this.ensureAlarmAt(now + BUFFER_IDLE_MS);
      const pair = new WebSocketPair();
      // The tags only mark the socket (keyed, and its grant); the agent's
      // secret key is never stored.
      const tags = ["role:" + role, "key:" + (role === "agent" ? "agent" : key)];
      if (role === "viewer") tags.push("grant:" + (grant ?? "legacy"));
      this.state.acceptWebSocket(pair[1], tags);
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
      // Invite and approver sockets are filtered before anything is
      // forwarded: malformed frames are dropped, frames outside the role's
      // list get a forbidden error. Owner and legacy sockets are not filtered.
      const grant = this.grantOf(ws);
      if (grant === "viewer" || grant === "approver") {
        const verdict = judgeViewerFrame(grant, message);
        if (verdict.verdict === "malformed") return;
        if (verdict.verdict === "forbidden") {
          try { ws.send(forbiddenFrame(verdict.id)); } catch { /* gone */ }
          return;
        }
      }
      // Every frame carries the socket's role to the serve, and no client
      // value survives: a tagged socket's non-object frame is dropped, a
      // legacy socket's `role` key is removed (see stampRelayRole).
      const relayedRole = grant === "owner" || grant === "viewer" || grant === "approver" ? grant : null;
      const outbound = stampRelayRole(message, relayedRole);
      if (outbound === null) return;
      if (outbound.length > limit) {
        try { ws.send(JSON.stringify({ type: "note", text: "frame too large (96KB limit) - dropped" })); } catch { /* gone */ }
        return;
      }
      // Viewer: {"type":"sync"} replays the buffer; anything else is
      // forwarded to the agent verbatim.
      let parsed: { type?: unknown } | null = null;
      try { parsed = JSON.parse(message) as { type?: unknown }; } catch { /* not json */ }
      // The web app (app/, via mobile/rust-core) opens with a protocol
      // `hello` and never sends `sync`, so its FIRST hello on a socket gets
      // the same replay. The hello itself is still forwarded below so the
      // agent answers with `welcome`. Only the first one replays, which
      // bounds the replay work per socket.
      //
      // Ordering (see cloud/bridge/NOTES.md "Replay and ordering"):
      // - The replay is written with ws.send BEFORE agent.send(hello). The
      //   agent's answers (welcome, then the Attach snapshot rc.rs sends on
      //   every hello) arrive as later events, so they reach this viewer
      //   after the replay.
      // - storage.list runs under the object's input gate, so the replay is
      //   one consistent prefix of the stored agent stream: no append is
      //   half-done and no later frame is mixed in.
      // - A live frame that reached this socket before its hello is not held
      //   back, so the replay may repeat it. The snapshot that follows the
      //   hello resets the client, so the repeat is transient and cannot
      //   leave a stale view behind.
      const isSync = parsed !== null && typeof parsed === "object" && parsed.type === "sync";
      const isFirstHello = parsed !== null && typeof parsed === "object" && parsed.type === "hello" && claimFirstHello(ws);
      if (isSync || isFirstHello) {
        const frames = await this.state.storage.list<StoredFrame>({ prefix: "frame:", limit: MAX_BUFFER });
        for (const frame of frames.values()) { try { ws.send(frame.text); } catch { break; } }
        if (isSync) return;
      }
      const now = Date.now();
      await this.state.storage.put("seen:viewer", now);
      const agent = this.agentSocket();
      try { agent?.send(outbound); } catch { /* gone */ }
      return;
    }
    // Keepalive pongs are not history: storing one every 30 s would evict
    // the whole replay buffer after about 100 idle minutes.
    if (isKeepalive(message)) {
      await this.state.storage.put("seen:agent", Date.now());
      return;
    }
    // A Notify (docs/PROTOCOL_V2.md section 6) also becomes a Web Push for
    // the subscribed devices. It runs in the background; the relay never waits.
    // Not awaited, and not handed to waitUntil: with push off the call is
    // skipped outright, so the relay path is unchanged.
    if (pushConfigured(this.env) && message.length <= MAX_FRAME && message.includes('"notify"')) {
      void this.pushNotify(message).catch((e: unknown) => {
        console.error("web push: notify not sent:", e instanceof Error ? e.name : "error");
      });
    }
    if (message.length <= MAX_FRAME) {
      await this.appendAgentFrame(message).catch(() => { /* storage hiccup - still fan out live */ });
    }
    for (const viewer of this.viewers()) { try { viewer.send(message); } catch { /* gone */ } }
  }

  // Web Push for one Notify frame. At most one push per kind every
  // PUSH_MIN_GAP_MS for this session; a 404 or 410 from the push service
  // removes that subscription. Endpoints and keys are never logged.
  async pushNotify(message: string): Promise<void> {
    const env = this.env;
    if (!pushConfigured(env)) return;
    const note = parseNotify(message);
    if (!note) return;
    const now = Date.now();
    const gate = "push:" + note.kind;
    const last = (await this.state.storage.get<number>(gate)) ?? 0;
    if (now - last < PUSH_MIN_GAP_MS) return;
    const targets = await directory(env).pushTargets(note.session, now);
    if (targets.subs.length === 0) return;
    await this.state.storage.put(gate, now);
    const keys = await importVapid(env.VAPID_PUBLIC_KEY!, env.VAPID_PRIVATE_KEY!, env.VAPID_SUBJECT!);
    const testOrigin = testPushOrigin(env);
    for (const sub of targets.subs) {
      if (!pushEndpointAllowed(sub.endpoint, testOrigin)) continue;
      const url = targets.session
        ? sub.origin + sessionOpenUrl(sub.origin.replace(/^http/, "ws"), targets.session.viewer_key)
        : sub.origin + "/app/";
      const payload = JSON.stringify({ title: targets.session?.title ?? "modron", body: note.text, data: { url } });
      let status: number;
      try {
        status = await sendPush(sub.endpoint, sub, payload, keys, now);
      } catch (e) {
        // Names only: the endpoint and the keys never reach the log.
        console.error("web push: send failed:", e instanceof Error ? e.name : "error");
        continue;
      }
      if (status === 404 || status === 410) await directory(env).deletePushSubscription(sub.id);
    }
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

// --- portal: owner sign-in, host registration, session directory, invites ---
// Owner routes are gated in order: OWNER_TOKEN set (else 503), then a live
// owner cookie (requireOwner). Host registration needs HOST_TOKEN. The
// Directory object is reached only through these handlers, never over HTTP.

const directory = (env: Env) => env.DIRECTORY.get(env.DIRECTORY.idFromName("directory"));
const iso = (ms: number) => new Date(ms).toISOString();

function plainResponse(status: number, text: string, extra: Record<string, string> = {}): Response {
  return new Response(text, {
    status,
    headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store", ...extra },
  });
}

function jsonResponse(status: number, value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

const portalOff = () => plainResponse(503, "portal not configured");

// Client address from Cloudflare's header (set on every proxied request).
function clientIp(request: Request): string {
  return (request.headers.get("cf-connecting-ip") ?? "unknown").slice(0, 64);
}

function bearerMatches(request: Request, expected: string): boolean {
  const auth = request.headers.get("authorization") ?? "";
  const presented = auth.startsWith("Bearer ") ? auth.slice("Bearer ".length) : "";
  return constantTimeEquals(presented, expected);
}

// The clock for one portal request: Date.now(), except that under
// MODRON_TEST_HOOKS an `x-test-now: <epoch ms>` header stands in for it, so
// test/portal.mjs can register a host that stopped heartbeating or one whose
// registration is past the 30-day sweep. Without the hook var the header is
// never read. Digits only; anything else is ignored.
export function requestNow(request: Request, env: Env): number {
  const header = env.MODRON_TEST_HOOKS ? request.headers.get("x-test-now") : null;
  if (header !== null && /^\d{1,15}$/.test(header)) return Number(header);
  return Date.now();
}

// Websocket origin of this request (wss for https, ws otherwise), the same
// scheme rule as appRedirectLocation.
function wsOriginOf(requestUrl: URL): string {
  const wsScheme = requestUrl.protocol === "https:" ? "wss:" : "ws:";
  return wsScheme + "//" + requestUrl.host;
}

// True when the request carries a live owner cookie (and refreshes it).
async function ownerCookieLive(request: Request, env: Env): Promise<boolean> {
  const cookie = readCookie(request, OWNER_COOKIE);
  if (cookie === null) return false;
  return directory(env).touchOwnerSession(await sha256Hex(cookie), Date.now());
}

// Used by every owner route. Returns null when signed in, else the 401.
export async function requireOwner(request: Request, env: Env): Promise<Response | null> {
  if (await ownerCookieLive(request, env)) return null;
  return plainResponse(401, "unauthorized");
}

// POST /auth { token }. Five attempts per minute per client IP, counted in
// the Directory before the token is compared.
async function handleAuth(request: Request, env: Env): Promise<Response> {
  const dir = directory(env);
  if (!(await dir.takeAuthAttempt(clientIp(request), Date.now()))) {
    return plainResponse(429, "too many attempts, try again in a minute", { "retry-after": "60" });
  }
  const parsed = await readJsonObject(request, ["token"]);
  if (!parsed.ok) return parsed.response;
  const presented = typeof parsed.body.token === "string" ? parsed.body.token : "";
  if (!constantTimeEquals(presented, env.OWNER_TOKEN ?? "")) return plainResponse(401, "unauthorized");
  const cookie = randomHex(32);
  await dir.createOwnerSession(await sha256Hex(cookie), Date.now());
  return new Response(JSON.stringify({ ok: true }), {
    headers: {
      "content-type": "application/json",
      "cache-control": "no-store",
      "set-cookie": ownerCookieHeader(cookie, !isLocalDev(new URL(request.url))),
    },
  });
}

// POST /auth/logout: revokes the presented cookie (if any) and clears it.
async function handleLogout(request: Request, env: Env): Promise<Response> {
  const cookie = readCookie(request, OWNER_COOKIE);
  if (cookie !== null) await directory(env).revokeOwnerSession(await sha256Hex(cookie));
  return new Response(null, {
    status: 204,
    headers: {
      "cache-control": "no-store",
      "set-cookie": ownerCookieHeader(null, !isLocalDev(new URL(request.url))),
    },
  });
}

// PUT | DELETE /api/host/sessions/<id>, bearer HOST_TOKEN.
async function handleHostSession(request: Request, env: Env, id: string): Promise<Response> {
  if (!env.HOST_TOKEN) return plainResponse(503, "host registration is disabled: this bridge has no HOST_TOKEN secret");
  if (!bearerMatches(request, env.HOST_TOKEN)) return plainResponse(401, "unauthorized");
  const sid = id.toLowerCase();
  if (!UUID_RE.test(sid)) return plainResponse(400, "invalid session id");
  const now = requestNow(request, env);
  if (request.method === "DELETE") {
    await directory(env).deleteSession(sid, now);
    return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
  }
  if (request.method !== "PUT") return plainResponse(405, "method not allowed");
  const parsed = await readJsonObject(request, ["title", "host_label", "host_kind", "viewer_key", "state"]);
  if (!parsed.ok) return parsed.response;
  const input = parseHostSession(parsed.body);
  if (!input.ok) return plainResponse(400, input.message);
  await directory(env).upsertSession({ id: sid, ...input.value, updated_at: now }, now);
  return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
}

// GET /api/sessions (owner), newest first. Each row carries online (a
// heartbeat within 5 minutes), updated_at as ISO 8601, and open_url: the
// path the owner's browser opens to attach to that session.
async function handleOwnerSessions(request: Request, env: Env): Promise<Response> {
  if (request.method !== "GET") return plainResponse(405, "method not allowed");
  const now = requestNow(request, env);
  const origin = wsOriginOf(new URL(request.url));
  const rows = await directory(env).listSessions(now);
  return jsonResponse(200, rows.map((row) => sessionView(row, now, origin)));
}

// GET /api/me (owner): { "owner": true } with a live owner cookie, else 401.
// The portal-off 503 is answered by the router before this runs.
async function handleMe(request: Request, env: Env): Promise<Response> {
  if (request.method !== "GET") return plainResponse(405, "method not allowed");
  const denied = await requireOwner(request, env);
  if (denied) return denied;
  return jsonResponse(200, { owner: true });
}

// POST /api/invites, GET /api/invites[?session_id=], DELETE /api/invites/<id>.
async function handleInvites(request: Request, env: Env, path: string): Promise<Response> {
  const dir = directory(env);
  const url = new URL(request.url);
  const ownerToken = env.OWNER_TOKEN ?? "";
  const hooks = Boolean(env.MODRON_TEST_HOOKS);
  const describe = async (r: { id: string; session_id: string; role: string; expires_at: number; revoked: number; created_at: number; uses: number }) => ({
    id: r.id,
    session_id: r.session_id,
    role: r.role,
    expires_at: iso(r.expires_at),
    created_at: iso(r.created_at),
    revoked: r.revoked === 1,
    uses: r.uses,
    url: `${url.origin}/i/${r.id}.${await inviteSignature(ownerToken, r.id)}`,
  });
  if (path === "/api/invites") {
    if (request.method === "POST") {
      const parsed = await readJsonObject(request, hooks
        ? ["session_id", "role", "ttl_hours", "ttl_seconds"]
        : ["session_id", "role", "ttl_hours"]);
      if (!parsed.ok) return parsed.response;
      const input = parseInvite(parsed.body, hooks);
      if (!input.ok) return plainResponse(400, input.message);
      const now = requestNow(request, env);
      if ((await dir.getSession(input.value.session_id, now)) === null) return plainResponse(404, "session is not registered");
      const id = randomHex(16);
      const expires_at = now + input.value.ttl_ms;
      await dir.createInvite({
        id,
        session_id: input.value.session_id,
        role: input.value.role,
        expires_at,
        revoked: 0,
        created_at: now,
        uses: 0,
      });
      return jsonResponse(201, {
        id,
        url: `${url.origin}/i/${id}.${await inviteSignature(ownerToken, id)}`,
        expires_at: iso(expires_at),
      });
    }
    if (request.method === "GET") {
      const sid = url.searchParams.get("session_id");
      if (sid !== null && !UUID_RE.test(sid)) return plainResponse(400, "invalid session_id");
      const rows = await dir.listInvites(sid === null ? null : sid.toLowerCase());
      return jsonResponse(200, await Promise.all(rows.map(describe)));
    }
    return plainResponse(405, "method not allowed");
  }
  const m = path.match(/^\/api\/invites\/([^/]+)$/);
  if (m) {
    if (request.method !== "DELETE") return plainResponse(405, "method not allowed");
    if (!INVITE_ID_RE.test(m[1])) return plainResponse(400, "invalid invite id");
    const existed = await dir.revokeInvite(m[1]);
    return existed
      ? new Response(null, { status: 204, headers: { "cache-control": "no-store" } })
      : plainResponse(404, "not found");
  }
  return plainResponse(404, "not found");
}

// GET /i/<id>.<sig>. Every failure is the same short page: 403 for a bad
// signature, 410 for expired, revoked or unregistered. A valid invite
// becomes a one-time ticket and a redirect to the app.
async function handleInviteLink(request: Request, env: Env, token: string): Promise<Response> {
  const notValid = (status: number) => new Response(
    "<!doctype html><meta charset=utf-8><title>modron</title><p>invite is not valid</p>",
    {
      status,
      headers: {
        "content-type": "text/html; charset=utf-8",
        "content-security-policy": "default-src 'none'",
        "cache-control": "no-store",
        "x-content-type-options": "nosniff",
      },
    },
  );
  const parsed = parseInviteToken(token);
  if (!parsed) return notValid(403);
  const expected = await inviteSignature(env.OWNER_TOKEN ?? "", parsed.id);
  if (!constantTimeEquals(expected, parsed.sig)) return notValid(403);
  const issued = await directory(env).issueTicket(parsed.id, Date.now());
  if (!issued) return notValid(410);
  const url = new URL(request.url);
  const wsScheme = url.protocol === "https:" ? "wss:" : "ws:";
  const inner = `${wsScheme}//${url.host}/ws/${issued.viewer_key}?ticket=${issued.ticket}`;
  return new Response(null, {
    status: 302,
    headers: { location: "/app/?url=" + encodeURIComponent(inner), "cache-control": "no-store" },
  });
}

// Grant for a viewer socket: a redeemed ticket (role from the invite), the
// owner cookie, or, with the portal off, nothing (the share key alone, as
// before). Anything else is 401 when the portal is on.
type Grant = { role: "owner" | "viewer" | "approver" | null };
async function viewerGrant(request: Request, env: Env, key: string): Promise<Grant | Response> {
  const ticket = new URL(request.url).searchParams.get("ticket");
  if (ticket !== null) {
    const denied = plainResponse(403, "ticket is not valid");
    if (!TICKET_RE.test(ticket)) return denied;
    const role = await directory(env).redeemTicket(ticket, key, Date.now());
    return role === null ? denied : { role };
  }
  if (!env.OWNER_TOKEN) return { role: null };
  if (await ownerCookieLive(request, env)) return { role: "owner" };
  return plainResponse(401, "unauthorized");
}

// Copy of the request with the grant header set to `role` (or none). Any
// client-supplied copy is removed first.
function withGrant(request: Request, role: string | null): Request {
  const headers = new Headers(request.headers);
  headers.delete(GRANT_HEADER);
  if (role !== null) headers.set(GRANT_HEADER, role);
  return new Request(request, { headers });
}

// --- web push: /api/push/key, /api/push/subscribe (README "Web push") ---

const PUSH_MIN_GAP_MS = 10 * 1000;

function pushConfigured(env: Env): boolean {
  return Boolean(env.VAPID_PUBLIC_KEY && env.VAPID_PRIVATE_KEY && env.VAPID_SUBJECT);
}

// The test origin for fake push services, honoured only under MODRON_TEST_HOOKS.
function testPushOrigin(env: Env): string | undefined {
  return env.MODRON_TEST_HOOKS ? env.PUSH_TEST_ORIGIN : undefined;
}

// Who may use the push routes: the owner cookie (every session), or an
// approver invite (its own session only). Viewer invites and no credential
// are refused. The returned scope is the session id, or null for the owner.
async function pushGrant(request: Request, env: Env, invite: string | null): Promise<{ scope: string | null } | Response> {
  if (await ownerCookieLive(request, env)) return { scope: null };
  if (invite === null) return plainResponse(401, "unauthorized");
  const parsed = parseInviteToken(invite);
  if (!parsed) return plainResponse(403, "invite is not valid");
  const expected = await inviteSignature(env.OWNER_TOKEN ?? "", parsed.id);
  if (!constantTimeEquals(expected, parsed.sig)) return plainResponse(403, "invite is not valid");
  const row = await directory(env).activeInvite(parsed.id, requestNow(request, env));
  if (!row) return plainResponse(410, "invite is not valid");
  if (row.role !== "approver") return plainResponse(403, "only owners and approver invites may subscribe");
  return { scope: row.session_id };
}

// GET /api/push/key[?invite=]: the VAPID public key the browser subscribes with.
async function handlePushKey(request: Request, env: Env): Promise<Response> {
  if (!pushConfigured(env)) return plainResponse(503, "web push is not configured");
  if (request.method !== "GET") return plainResponse(405, "method not allowed");
  const grant = await pushGrant(request, env, new URL(request.url).searchParams.get("invite"));
  if (grant instanceof Response) return grant;
  return jsonResponse(200, { key: env.VAPID_PUBLIC_KEY });
}

// POST /api/push/subscribe { endpoint, keys: { p256dh, auth }, invite? } and
// DELETE /api/push/subscribe { endpoint, invite? }. Stored in the Directory
// under a SHA-256 of the endpoint.
async function handlePushSubscribe(request: Request, env: Env): Promise<Response> {
  if (!pushConfigured(env)) return plainResponse(503, "web push is not configured");
  if (request.method !== "POST" && request.method !== "DELETE") return plainResponse(405, "method not allowed");
  const parsed = await readJsonObject(request, ["endpoint", "keys", "invite"]);
  if (!parsed.ok) return parsed.response;
  const body = parsed.body;
  if (typeof body.endpoint !== "string") return plainResponse(400, "endpoint must be a string");
  if (body.invite !== undefined && typeof body.invite !== "string") return plainResponse(400, "invite must be a string");
  const grant = await pushGrant(request, env, typeof body.invite === "string" ? body.invite : null);
  if (grant instanceof Response) return grant;
  const id = await sha256Hex(body.endpoint);
  if (request.method === "DELETE") {
    await directory(env).deletePushSubscription(id);
    return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
  }
  if (!pushEndpointAllowed(body.endpoint, testPushOrigin(env))) {
    return plainResponse(400, "endpoint is not a supported push service");
  }
  const keys = body.keys;
  if (keys === null || typeof keys !== "object" || Array.isArray(keys)) return plainResponse(400, "keys must be an object");
  const k = keys as Record<string, unknown>;
  if (Object.keys(k).some((f) => f !== "p256dh" && f !== "auth")) return plainResponse(400, "unknown key field");
  if (typeof k.p256dh !== "string" || typeof k.auth !== "string" || !validSubscriptionKeys(k.p256dh, k.auth)) {
    return plainResponse(400, "keys must hold a p256dh point and an auth secret");
  }
  await directory(env).upsertPushSubscription({
    id,
    endpoint: body.endpoint,
    origin: new URL(request.url).origin,
    p256dh: k.p256dh,
    auth: k.auth,
    role: grant.scope === null ? "owner" : "approver",
    session_id: grant.scope,
    created_at: requestNow(request, env),
  });
  return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
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
    // With the portal on, the root is the app (sign-in or the session list).
    // Without it, the plain text answer stays as it was.
    if (path === "/" && (request.method === "GET" || request.method === "HEAD")) {
      if (env.OWNER_TOKEN) {
        return new Response(null, { status: 302, headers: { location: "/app/", "cache-control": "no-store" } });
      }
    }
    if (path === "/") return new Response("modron remote-control bridge - open a /s/<key> share link from modron");
    // The web push receiver (README "Web push"). Served here, not from the
    // bundle, so the bundle needs no extra file. Revalidated on every load.
    if (path === "/app/sw.js" && (request.method === "GET" || request.method === "HEAD")) {
      return new Response(request.method === "HEAD" ? null : PUSH_WORKER, {
        headers: {
          "content-type": "application/javascript; charset=utf-8",
          "cache-control": "no-cache",
          "x-content-type-options": "nosniff",
        },
      });
    }
    // The unified web app (static assets). Anything not in the bundle is a
    // plain 404 from the assets binding.
    if (path === "/app" || path.startsWith("/app/")) {
      if (path === "/app") return Response.redirect(new URL(request.url).origin + "/app/", 301);
      return serveApp(request, env);
    }
    // Portal routes. Anything under /api/, /auth, /i/ needs OWNER_TOKEN.
    if (path === "/auth" || path === "/auth/logout" || path.startsWith("/api/") || path.startsWith("/i/")) {
      if (!env.OWNER_TOKEN) return portalOff();
    }
    if (path === "/auth") {
      return request.method === "POST" ? handleAuth(request, env) : plainResponse(405, "method not allowed");
    }
    if (path === "/auth/logout") {
      return request.method === "POST" ? handleLogout(request, env) : plainResponse(405, "method not allowed");
    }
    if (path === "/api/me") return handleMe(request, env);
    if (path === "/api/sessions") {
      const denied = await requireOwner(request, env);
      if (denied) return denied;
      return handleOwnerSessions(request, env);
    }
    if ((m = path.match(/^\/api\/host\/sessions\/([^/]+)$/))) {
      return handleHostSession(request, env, m[1]);
    }
    if (path === "/api/invites" || path.startsWith("/api/invites/")) {
      const denied = await requireOwner(request, env);
      if (denied) return denied;
      return handleInvites(request, env, path);
    }
    if ((m = path.match(/^\/i\/([^/]+)$/)) && request.method === "GET") {
      return handleInviteLink(request, env, m[1]);
    }
    if (path === "/api/push/key") return handlePushKey(request, env);
    if (path === "/api/push/subscribe") return handlePushSubscribe(request, env);
    if (path.startsWith("/api/")) return plainResponse(404, "not found");
    // The agent tunnel: /agent/<agent key>, routed to the DO named by the
    // derived viewer key (see viewerKeyOf). With HOST_TOKEN set, the bearer
    // is checked here, before the key or any Durable Object is touched.
    if ((m = path.match(/^\/agent\/([^/]+)$/))) {
      if (env.HOST_TOKEN && !bearerMatches(request, env.HOST_TOKEN)) return plainResponse(401, "unauthorized");
      if (!validKey(m[1])) return new Response("invalid key", { status: 400 });
      const viewerKey = await viewerKeyOf(m[1]);
      return env.BRIDGE.get(env.BRIDGE.idFromName(viewerKey)).fetch(request);
    }
    // Viewers: /s/<viewer key> redirects to the web app; /s/<key>?classic=1
    // keeps the old inline viewer page as a fallback; /ws/<viewer key> is
    // the viewer socket.
    if ((m = path.match(/^\/s\/([^/]+)$/))) {
      if (!validKey(m[1])) return new Response("invalid key", { status: 400 });
      if (new URL(request.url).searchParams.get("classic") === "1") {
        return env.BRIDGE.get(env.BRIDGE.idFromName(m[1])).fetch(request);
      }
      return new Response(null, {
        status: 302,
        headers: { location: appRedirectLocation(new URL(request.url), m[1]) },
      });
    }
    if ((m = path.match(/^\/ws\/([^/]+)$/))) {
      if (!validKey(m[1])) return new Response("invalid key", { status: 400 });
      // The ticket is redeemed (or the owner cookie checked) here, before any
      // upgrade, and the grant travels to the object in a header.
      const grant = await viewerGrant(request, env, m[1]);
      if (grant instanceof Response) return grant;
      return env.BRIDGE.get(env.BRIDGE.idFromName(m[1])).fetch(withGrant(request, grant.role));
    }
    return new Response("modron bridge - see cloud/bridge/README.md", { status: 404 });
  },
};

export default router;
