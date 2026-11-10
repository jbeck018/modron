// modron remote-control bridge — a transport-only relay.
//
// One Durable Object instance per share key. The agent side (`modron
// serve` after /remote-control) connects to /agent/<key>; viewers open
// /s/<key> (web page, upgrading /ws/<key>). Text frames relay verbatim:
// agent -> every viewer (buffered for late joiners), viewer -> agent.
// The bridge never interprets Frame contents.
//
// Storage: the last 200 agent frames persist in DO storage, so a viewer
// opening the link after work started still sees the transcript so far.
// POC trust model: possession of the share key is the credential, like
// Claude Code share links.

interface Env {
  BRIDGE: DurableObjectNamespace<SessionBridge>;
}

// The DurableObject base provides the hibernation plumbing brand.
import { DurableObject } from "cloudflare:workers";

/// The slice of DurableObjectContext this bridge uses.
interface BridgeContext {
  getTags(ws: WebSocket): string[];
  getWebSockets(tag: string): WebSocket[];
  acceptWebSocket(ws: WebSocket, tags?: string[]): void;
  storage: {
    get<T>(key: string): Promise<T | undefined>;
    put<T>(key: string, value: T): Promise<void>;
  };
}

// One DO instance per share key: it owns the agent tunnel, the viewers,
// and the replay buffer for that session.
const router: ExportedHandler<Env> = {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/") {
      return new Response(
        "modron remote-control bridge - open a /s/<key> share link from modron"
      );
    }
    const key = url.pathname.split("/").pop() || "";
    if (!key) return new Response("missing key", { status: 400 });
    const stub = env.BRIDGE.get(env.BRIDGE.idFromName(key));
    return stub.fetch(request);
  },
};

export default router;

const MAX_BUFFER = 200;
const MAX_FRAME = 1 << 20; // 1 MiB per message, matching the protocol doc
const BUFFER_STORAGE_KEY = "agent-buffer";

interface BufferedFrame {
  text: string;
  ts: number;
}

interface BridgeDeps {
  state: DurableObjectState;
}

export class SessionBridge extends DurableObject {
  declare private state: BridgeContext;

  constructor(ctx: BridgeContext, _env: Env) {
    super(ctx as never, _env);
    this.state = ctx;
  }

  private keyOf(ws: WebSocket): string {
    for (const tag of this.state.getTags(ws)) {
      if (tag.startsWith("key:")) return tag.slice(4);
    }
    return "";
  }

  private roleOf(ws: WebSocket): string {
    for (const tag of this.state.getTags(ws)) {
      if (tag.startsWith("role:")) return tag.slice(5);
    }
    return "";
  }

  private viewers(key: string): WebSocket[] {
    return this.state.getWebSockets(`role:viewer`);
  }

  private agent(key: string): WebSocket | null {
    const sockets = this.state.getWebSockets(`role:agent`);
    return sockets.at(-1) ?? null; // newest tunnel wins
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;

    // Web viewer page (no upgrade header).
    if (path.startsWith("/s/") && request.headers.get("upgrade") !== "websocket") {
      const key = path.slice(3);
      if (!key) return new Response("missing key", { status: 400 });
      return new Response(viewerPage(key), {
        headers: { "content-type": "text/html; charset=utf-8" },
      });
    }

    // WebSocket upgrades: /agent/<key> (serve side), /ws/<key> (viewer).
    if (
      (path.startsWith("/agent/") || path.startsWith("/ws/")) &&
      request.headers.get("upgrade") === "websocket"
    ) {
      const key = path.split("/").pop() || "";
      if (!key) return new Response("missing key", { status: 400 });
      const role = path.startsWith("/agent/") ? "agent" : "viewer";
      const pair = new WebSocketPair();
      // ALL sockets use hibernation: getWebSockets(tag) only reports
      // hibernation sockets, so mixing plain accept() breaks relaying.
      // Late joiners get the buffered transcript via webSocketOpen.
      this.state.acceptWebSocket(pair[1], [`role:${role}`, `key:${key}`]);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    return new Response("modron bridge - see cloud/bridge/README.md", {
      status: 404,
    });
  }

  // Hibernatable WebSocket events (the DO wakes per message/close).
  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer) {
    if (typeof message !== "string" || message.length > MAX_FRAME) return;
    const key = this.keyOf(ws);
    if (!key) return;

    // Late-join replay: the viewer asks with {type:"sync"} right after
    // open (webSocketOpen doesn't fire in wrangler dev, so the page
    // sends this instead - works identically in dev and prod).
    if (this.roleOf(ws) !== "agent" && message.includes('"sync"')) {
      const buffered = await this.buffer();
      for (const frame of buffered) {
        try {
          ws.send(frame.text);
        } catch {
          /* viewer gone during replay */
        }
      }
      return;
    }

    if (this.roleOf(ws) === "agent") {
      // Buffer agent->viewer frames for late joiners, then relay.
      const buffered = await this.buffer();
      buffered.push({ text: message, ts: Date.now() });
      while (buffered.length > MAX_BUFFER) buffered.shift();
      await this.state.storage.put(BUFFER_STORAGE_KEY, buffered);
      for (const viewer of this.viewers(key)) {
        try {
          viewer.send(message);
        } catch {
          /* viewer gone */
        }
      }
      return;
    }

    // Viewer -> agent (submit/approve/attach/...): point-in-time, the
    // agent re-attaches on its own reconnect, so no buffering.
    const agent = this.agent(key);
    try {
      agent?.send(message);
    } catch {
      /* agent gone */
    }
  }

  async webSocketClose(ws: WebSocket) {
    const key = this.keyOf(ws);
    if (!key) return;
    if (this.roleOf(ws) === "agent") {
      // The tunnel dropped: tell viewers to re-arm via /remote-control.
      for (const viewer of this.viewers(key)) {
        try {
          viewer.send(
            JSON.stringify({
              type: "note",
              text: "agent tunnel closed - run /remote-control again",
            })
          );
        } catch {
          /* viewer gone */
        }
      }
    }
  }

  async buffer(): Promise<BufferedFrame[]> {
    return (
      (await this.state.storage.get<BufferedFrame[]>(BUFFER_STORAGE_KEY)) ?? []
    );
  }
}

function viewerPage(key: string): string {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>modron - ${key}</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; background: #111; color: #eee; font: 14px/1.5 -apple-system,
         "Segoe UI", Roboto, sans-serif; display: flex; flex-direction: column;
         height: 100dvh; }
  header { padding: 10px 16px; border-bottom: 1px solid #333; display: flex;
           gap: 10px; align-items: center; }
  header .dot { width: 10px; height: 10px; border-radius: 50%; background: #888; }
  header .dot.on { background: #4caf50; }
  header b { font-weight: 600; }
  #log { flex: 1; overflow-y: auto; padding: 16px; display: flex;
         flex-direction: column; gap: 6px; }
  #log .assistant { white-space: pre-wrap; }
  #log .user { align-self: flex-end; background: #2a2a2a; padding: 6px 12px;
               border-radius: 12px; }
  #log .meta { color: #888; font-size: 12px; }
  #log .tool { color: #7ec27e; font-size: 12px; }
  .approval { border: 1px solid #444; border-radius: 8px; padding: 8px;
              display: flex; gap: 8px; align-items: center; }
  .approval button { padding: 4px 10px; border-radius: 6px; border: 1px solid
                     #555; background: #222; color: #eee; }
  .approval button.allow { background: #2e7d32; border-color: #2e7d32; }
  .approval button.deny { background: #b71c1c; border-color: #b71c1c; }
  form { display: flex; gap: 8px; padding: 12px 16px; border-top: 1px solid
         #333; }
  form input { flex: 1; background: #1c1c1c; color: #eee; border: 1px solid
               #444; border-radius: 10px; padding: 10px 14px; font: inherit; }
  form button { background: #2e7d32; color: #fff; border: none;
                border-radius: 10px; padding: 0 18px; font: inherit; }
  .disconnected { color: #e57373; }
</style>
</head>
<body>
<header><span class="dot" id="dot"></span><b>modron</b>
  <span style="color:#888">session ${key}</span></header>
<div id="log"><div class="meta">connecting...</div></div>
<form id="composer"><input id="task" placeholder="Type a task; enter to send"
  autocomplete="off"><button type="submit">send</button></form>
<script>
const key = ${JSON.stringify(key)};
const proto = location.protocol === "https:" ? "wss:" : "ws:";
const ws = new WebSocket(proto + "//" + location.host + "/ws/" + key);
const dot = document.getElementById("dot");
const log = document.getElementById("log");
function line(cls, text) {
  const el = document.createElement("div");
  el.className = cls;
  el.textContent = text;
  log.appendChild(el);
  log.scrollTop = log.scrollHeight;
}
function renderEvent(event) {
  switch (event.type) {
    case "task": return line("user", event.text);
    case "message":
      if (event.message && event.message.role === "assistant" &&
          event.message.content) line("assistant", event.message.content);
      return;
    case "tool_result": return;
    case "usage": case "turn_trace": case "checkpoint": case "model_selected":
      return;
    default: return;
  }
}
ws.onopen = () => { dot.classList.add("on"); line("meta", "connected"); };
ws.onclose = () => { dot.classList.remove("on");
  line("meta disconnected", "tunnel closed"); };
ws.onmessage = (m) => {
  let f;
  try { f = JSON.parse(m.data); } catch { return; }
  switch (f.type) {
    case "snapshot": {
      log.innerHTML = "";
      const sid = f.session_id ? String(f.session_id).slice(0, 8) : "?";
      line("meta", "attached to session " + sid);
      if (f.live && f.live.running) line("meta", "run in progress...");
      (f.events || []).forEach(renderEvent);
      break;
    }
    case "event": renderEvent(f.event); break;
    case "delta":
      if (f.delta && f.delta.kind === "text") line("assistant", f.delta.text);
      break;
    case "tool":
      if (f.activity) line("tool",
        (f.activity.kind === "start" ? "tool: " : "tool done: ") +
        f.activity.name);
      break;
    case "approval_requested": {
      line("approval", "approval needed: " + (f.call ? f.call.name : "?"));
      const row = document.createElement("div");
      row.className = "approval";
      [["allow_once", "allow", "allow"], ["deny", "deny", "deny"]]
        .forEach(([decision, label, cls]) => {
          const b = document.createElement("button");
          b.textContent = label;
          b.className = cls;
          b.onclick = () => ws.send(JSON.stringify(
            { type: "approve", request_id: f.request_id, decision }));
          row.appendChild(b);
        });
      log.appendChild(row);
      log.scrollTop = log.scrollHeight;
      break;
    }
    case "turn_done": line("meta", "turn complete"); break;
    case "turn_aborted": line("meta", "interrupted"); break;
    case "turn_failed": line("meta", "failed: " + (f.error || "")); break;
    case "note": line("meta", f.text); break;
  }
};
document.getElementById("composer").onsubmit = (e) => {
  e.preventDefault();
  const input = document.getElementById("task");
  const task = input.value.trim();
  if (!task) return;
  ws.send(JSON.stringify({ type: "submit", task, session_id: null }));
  line("user", task);
  input.value = "";
};
</script>
</body>
</html>`;
}
