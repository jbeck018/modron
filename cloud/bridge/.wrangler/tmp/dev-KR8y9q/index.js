var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: true });

// src/index.ts
import { DurableObject } from "cloudflare:workers";
var router = {
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
  }
};
var src_default = router;
var MAX_BUFFER = 200;
var MAX_FRAME = 1 << 20;
var BUFFER_STORAGE_KEY = "agent-buffer";
var SessionBridge = class extends DurableObject {
  static {
    __name(this, "SessionBridge");
  }
  constructor(ctx, _env) {
    super(ctx, _env);
    this.state = ctx;
  }
  keyOf(ws) {
    for (const tag of this.state.getTags(ws)) {
      if (tag.startsWith("key:")) return tag.slice(4);
    }
    return "";
  }
  roleOf(ws) {
    for (const tag of this.state.getTags(ws)) {
      if (tag.startsWith("role:")) return tag.slice(5);
    }
    return "";
  }
  viewers(key) {
    return this.state.getWebSockets(`role:viewer`);
  }
  agent(key) {
    const sockets = this.state.getWebSockets(`role:agent`);
    return sockets.at(-1) ?? null;
  }
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname;
    if (path.startsWith("/s/") && request.headers.get("upgrade") !== "websocket") {
      const key = path.slice(3);
      if (!key) return new Response("missing key", { status: 400 });
      return new Response(viewerPage(key), {
        headers: { "content-type": "text/html; charset=utf-8" }
      });
    }
    if ((path.startsWith("/agent/") || path.startsWith("/ws/")) && request.headers.get("upgrade") === "websocket") {
      const key = path.split("/").pop() || "";
      if (!key) return new Response("missing key", { status: 400 });
      const role = path.startsWith("/agent/") ? "agent" : "viewer";
      const pair = new WebSocketPair();
      this.state.acceptWebSocket(pair[1], [`role:${role}`, `key:${key}`]);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    return new Response("modron bridge - see cloud/bridge/README.md", {
      status: 404
    });
  }
  // Hibernatable WebSocket events (the DO wakes per message/close).
  async webSocketMessage(ws, message) {
    if (typeof message !== "string" || message.length > MAX_FRAME) return;
    const key = this.keyOf(ws);
    if (!key) return;
    if (this.roleOf(ws) !== "agent" && message.includes('"sync"')) {
      const buffered = await this.buffer();
      for (const frame of buffered) {
        try {
          ws.send(frame.text);
        } catch {
        }
      }
      return;
    }
    if (this.roleOf(ws) === "agent") {
      const buffered = await this.buffer();
      buffered.push({ text: message, ts: Date.now() });
      while (buffered.length > MAX_BUFFER) buffered.shift();
      await this.state.storage.put(BUFFER_STORAGE_KEY, buffered);
      for (const viewer of this.viewers(key)) {
        try {
          viewer.send(message);
        } catch {
        }
      }
      return;
    }
    const agent = this.agent(key);
    try {
      agent?.send(message);
    } catch {
    }
  }
  async webSocketClose(ws) {
    const key = this.keyOf(ws);
    if (!key) return;
    if (this.roleOf(ws) === "agent") {
      for (const viewer of this.viewers(key)) {
        try {
          viewer.send(
            JSON.stringify({
              type: "note",
              text: "agent tunnel closed - run /remote-control again"
            })
          );
        } catch {
        }
      }
    }
  }
  async buffer() {
    return await this.state.storage.get(BUFFER_STORAGE_KEY) ?? [];
  }
};
function viewerPage(key) {
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
<\/script>
</body>
</html>`;
}
__name(viewerPage, "viewerPage");

// node_modules/wrangler/templates/middleware/middleware-ensure-req-body-drained.ts
var drainBody = /* @__PURE__ */ __name(async (request, env, _ctx, middlewareCtx) => {
  try {
    return await middlewareCtx.next(request, env);
  } finally {
    try {
      if (request.body !== null && !request.bodyUsed) {
        const reader = request.body.getReader();
        while (!(await reader.read()).done) {
        }
      }
    } catch (e) {
      console.error("Failed to drain the unused request body.", e);
    }
  }
}, "drainBody");
var middleware_ensure_req_body_drained_default = drainBody;

// node_modules/wrangler/templates/middleware/middleware-miniflare3-json-error.ts
function reduceError(e) {
  return {
    name: e?.name,
    message: e?.message ?? String(e),
    stack: e?.stack,
    cause: e?.cause === void 0 ? void 0 : reduceError(e.cause)
  };
}
__name(reduceError, "reduceError");
var jsonError = /* @__PURE__ */ __name(async (request, env, _ctx, middlewareCtx) => {
  try {
    return await middlewareCtx.next(request, env);
  } catch (e) {
    const error = reduceError(e);
    const body = JSON.stringify(error);
    const headers = {
      "Content-Type": "application/json",
      "MF-Experimental-Error-Stack": "true"
    };
    const encoded = encodeURIComponent(body);
    if (encoded.length <= 8192) {
      headers["MF-Experimental-Error-Stack-Payload"] = encoded;
    }
    return new Response(body, { status: 500, headers });
  }
}, "jsonError");
var middleware_miniflare3_json_error_default = jsonError;

// .wrangler/tmp/bundle-JxAe6E/middleware-insertion-facade.js
var __INTERNAL_WRANGLER_MIDDLEWARE__ = [
  middleware_ensure_req_body_drained_default,
  middleware_miniflare3_json_error_default
];
var middleware_insertion_facade_default = src_default;

// node_modules/wrangler/templates/middleware/common.ts
var __facade_middleware__ = [];
function __facade_register__(...args) {
  __facade_middleware__.push(...args.flat());
}
__name(__facade_register__, "__facade_register__");
function __facade_invokeChain__(request, env, ctx, dispatch, middlewareChain) {
  const [head, ...tail] = middlewareChain;
  const middlewareCtx = {
    dispatch,
    next(newRequest, newEnv) {
      return __facade_invokeChain__(newRequest, newEnv, ctx, dispatch, tail);
    }
  };
  return head(request, env, ctx, middlewareCtx);
}
__name(__facade_invokeChain__, "__facade_invokeChain__");
function __facade_invoke__(request, env, ctx, dispatch, finalMiddleware) {
  return __facade_invokeChain__(request, env, ctx, dispatch, [
    ...__facade_middleware__,
    finalMiddleware
  ]);
}
__name(__facade_invoke__, "__facade_invoke__");

// .wrangler/tmp/bundle-JxAe6E/middleware-loader.entry.ts
var __Facade_ScheduledController__ = class ___Facade_ScheduledController__ {
  constructor(scheduledTime, cron, noRetry) {
    this.scheduledTime = scheduledTime;
    this.cron = cron;
    this.#noRetry = noRetry;
  }
  scheduledTime;
  cron;
  static {
    __name(this, "__Facade_ScheduledController__");
  }
  #noRetry;
  noRetry() {
    if (!(this instanceof ___Facade_ScheduledController__)) {
      throw new TypeError("Illegal invocation");
    }
    this.#noRetry();
  }
};
function wrapExportedHandler(worker) {
  if (__INTERNAL_WRANGLER_MIDDLEWARE__ === void 0 || __INTERNAL_WRANGLER_MIDDLEWARE__.length === 0) {
    return worker;
  }
  for (const middleware of __INTERNAL_WRANGLER_MIDDLEWARE__) {
    __facade_register__(middleware);
  }
  const fetchDispatcher = /* @__PURE__ */ __name(function(request, env, ctx) {
    if (worker.fetch === void 0) {
      throw new Error("Handler does not export a fetch() function.");
    }
    return worker.fetch(request, env, ctx);
  }, "fetchDispatcher");
  return {
    ...worker,
    fetch(request, env, ctx) {
      const dispatcher = /* @__PURE__ */ __name(function(type, init) {
        if (type === "scheduled" && worker.scheduled !== void 0) {
          const controller = new __Facade_ScheduledController__(
            Date.now(),
            init.cron ?? "",
            () => {
            }
          );
          return worker.scheduled(controller, env, ctx);
        }
      }, "dispatcher");
      return __facade_invoke__(request, env, ctx, dispatcher, fetchDispatcher);
    }
  };
}
__name(wrapExportedHandler, "wrapExportedHandler");
function wrapWorkerEntrypoint(klass) {
  if (__INTERNAL_WRANGLER_MIDDLEWARE__ === void 0 || __INTERNAL_WRANGLER_MIDDLEWARE__.length === 0) {
    return klass;
  }
  for (const middleware of __INTERNAL_WRANGLER_MIDDLEWARE__) {
    __facade_register__(middleware);
  }
  return class extends klass {
    #fetchDispatcher = /* @__PURE__ */ __name((request, env, ctx) => {
      this.env = env;
      this.ctx = ctx;
      if (super.fetch === void 0) {
        throw new Error("Entrypoint class does not define a fetch() function.");
      }
      return super.fetch(request);
    }, "#fetchDispatcher");
    #dispatcher = /* @__PURE__ */ __name((type, init) => {
      if (type === "scheduled" && super.scheduled !== void 0) {
        const controller = new __Facade_ScheduledController__(
          Date.now(),
          init.cron ?? "",
          () => {
          }
        );
        return super.scheduled(controller);
      }
    }, "#dispatcher");
    fetch(request) {
      return __facade_invoke__(
        request,
        this.env,
        this.ctx,
        this.#dispatcher,
        this.#fetchDispatcher
      );
    }
  };
}
__name(wrapWorkerEntrypoint, "wrapWorkerEntrypoint");
var WRAPPED_ENTRY;
if (typeof middleware_insertion_facade_default === "object") {
  WRAPPED_ENTRY = wrapExportedHandler(middleware_insertion_facade_default);
} else if (typeof middleware_insertion_facade_default === "function") {
  WRAPPED_ENTRY = wrapWorkerEntrypoint(middleware_insertion_facade_default);
}
var middleware_loader_entry_default = WRAPPED_ENTRY;
export {
  SessionBridge,
  __INTERNAL_WRANGLER_MIDDLEWARE__,
  middleware_loader_entry_default as default
};
//# sourceMappingURL=index.js.map
