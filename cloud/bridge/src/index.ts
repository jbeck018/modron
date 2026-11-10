// modron remote-control bridge - a transport-only relay.
// One Durable Object instance per share key. Agent connects to
// /agent/<key>; viewers open /s/<key> then /ws/<key>.
// Pages: POST /pages/<key> stores, GET /p/<id> renders.
// Trust model: possession of the share key is the credential.

const MAX_BUFFER = 200;
const BUFFER_KEY = "agent-buffer";
const MAX_FRAME = 1048576;

import { DurableObject } from "cloudflare:workers";

interface Env {
  BRIDGE: DurableObjectNamespace<SessionBridge>;
}

export class SessionBridge extends DurableObject {
  state: DurableObjectState;
  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.state = state;
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
  async buffer() { return (await this.state.storage.get<{ text: string; ts: number }[]>(BUFFER_KEY)) ?? []; }

  async handlePages(request: Request): Promise<Response> {
    if (request.method === "POST") {
      let body: { title?: string; content_md?: string };
      try { body = await request.json() as { title?: string; content_md?: string }; }
      catch { return new Response("bad json", { status: 400 }); }
      const id = Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
      await this.state.storage.put("page:" + id, { title: body.title || "modron page", content_md: body.content_md || "" });
      const origin = new URL(request.url).origin;
      return new Response(JSON.stringify({ id, url: origin + "/p/" + id }), { headers: { "content-type": "application/json" } });
    }
    const id = new URL(request.url).pathname.slice(3);
    const page = await this.state.storage.get<{ title: string; content_md: string }>("page:" + id);
    if (!page) return new Response("not found", { status: 404 });
    const esc = (t: string) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;");
    const paras = page.content_md.split(/\n\n/).map((p) => "<p>" + esc(p).replace(/\n/g, "<br>") + "</p>").join("");
    return new Response("<!doctype html><html><head><meta charset=utf-8><title>" + esc(page.title) + "</title></head><body><h1>" + esc(page.title) + "</h1>" + paras + "</body></html>", { headers: { "content-type": "text/html; charset=utf-8" } });
  }

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path.startsWith("/pages/") && request.method === "POST") return this.handlePages(request);
    if (path.startsWith("/p/") && request.method === "GET") return this.handlePages(request);
    if ((path.startsWith("/agent/") || path.startsWith("/ws/")) && request.headers.get("upgrade") === "websocket") {
      const key = path.split("/").pop() || "";
      if (!key) return new Response("missing key", { status: 400 });
      const role = path.startsWith("/agent/") ? "agent" : "viewer";
      const pair = new WebSocketPair();
      this.state.acceptWebSocket(pair[1], ["role:" + role, "key:" + key]);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    return new Response("modron bridge - see cloud/bridge/README.md", { status: 404 });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    if (typeof message !== "string" || message.length > MAX_FRAME) return;
    const key = this.keyOf(ws);
    if (!key) return;
    if (this.roleOf(ws) !== "agent" && message.includes('"sync"')) {
      const buffered = await this.buffer();
      for (const frame of buffered) { try { ws.send(frame.text); } catch { /* gone */ } }
      return;
    }
    if (this.roleOf(ws) === "agent") {
      const buffered = await this.buffer();
      buffered.push({ text: message, ts: Date.now() });
      while (buffered.length > MAX_BUFFER) buffered.shift();
      await this.state.storage.put(BUFFER_KEY, buffered);
      for (const viewer of this.viewers()) { try { viewer.send(message); } catch { /* gone */ } }
      return;
    }
    const agent = this.agentSocket();
    try { agent?.send(message); } catch { /* gone */ }
  }

  async webSocketClose(ws: WebSocket) {
    const key = this.keyOf(ws);
    if (!key) return;
    if (this.roleOf(ws) === "agent") {
      for (const viewer of this.viewers()) { try { viewer.send(JSON.stringify({ type: "note", text: "agent tunnel closed - run /remote-control again" })); } catch { /* gone */ } }
    }
  }
}

const router: ExportedHandler<Env> = {
  async fetch(request: Request, env: Env): Promise<Response> {
    const path = new URL(request.url).pathname;
    // Pages live in a dedicated DO so /p/<id> can find them regardless
    // of which session tunnel they came from.
    if (path.startsWith("/pages/") || path.startsWith("/p/")) {
      const stub = env.BRIDGE.get(env.BRIDGE.idFromName("__pages__"));
      return stub.fetch(request);
    }
    if (path === "/") return new Response("modron remote-control bridge - open a /s/<key> share link from modron");
    const key = path.split("/").pop() || "";
    if (!key) return new Response("missing key", { status: 400 });
    const stub = env.BRIDGE.get(env.BRIDGE.idFromName(key));
    return stub.fetch(request);
  },
};

export default router;
