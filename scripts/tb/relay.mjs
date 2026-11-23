#!/usr/bin/env bun
// Model API relay for agent-vs-agent benchmark runs.
//
// Task containers talk to this relay instead of the provider. That gives:
//   - one place that holds the credential (containers never see the key);
//   - the same token accounting for every agent, read from the provider's
//     own usage fields rather than from each agent's self-report;
//   - an audit of which models each trial really called.
//
// Routes:
//   /t/<tag>/<path>   forwarded to <RELAY_UPSTREAM>/<path>, counted under <tag>
//   /stats            all tags as JSON
//   /stats/<tag>      one tag as JSON
//
// Env:
//   RELAY_PORT      listen port (default 8787)
//   RELAY_UPSTREAM  provider origin (default https://api.z.ai)
//   RELAY_API_KEY   when set, replaces the client's Authorization and
//                   x-api-key headers; when unset the client's are passed on
//   RELAY_STATS     file the stats are written to after every response
//
// Run with bun (its fetch honors HTTPS_PROXY); node 22+ also works when no
// outbound proxy is needed.

import http from "node:http";
import fs from "node:fs";

const PORT = Number(process.env.RELAY_PORT || 8787);
const UPSTREAM = (process.env.RELAY_UPSTREAM || "https://api.z.ai").replace(/\/+$/, "");
const API_KEY = process.env.RELAY_API_KEY || "";
const STATS_FILE = process.env.RELAY_STATS || "";

const HOP_BY_HOP = new Set([
  "host", "connection", "content-length", "transfer-encoding", "keep-alive",
  "proxy-authorization", "proxy-connection", "upgrade", "accept-encoding",
]);

/** tag -> { requests, errors, input, output, cached, models: {id: requests} } */
const stats = new Map();

function entry(tag) {
  let row = stats.get(tag);
  if (!row) {
    row = { requests: 0, errors: 0, input: 0, output: 0, cached: 0, models: {} };
    stats.set(tag, row);
  }
  return row;
}

function persist() {
  if (!STATS_FILE) return;
  try {
    fs.writeFileSync(STATS_FILE, JSON.stringify(Object.fromEntries(stats), null, 1));
  } catch {
    // Stats are best-effort; a full disk must not fail a model call.
  }
}

/**
 * Fold one usage object into {input, output, cached}. Handles both wire
 * shapes: OpenAI chat (prompt_tokens includes cached tokens) and Anthropic
 * messages (input_tokens excludes cache reads and writes). `input` is
 * normalized to ALL prompt tokens, cached ones included.
 */
export function foldUsage(acc, usage) {
  if (!usage || typeof usage !== "object") return acc;
  const num = (value) => (Number.isFinite(value) ? value : 0);
  let input = 0;
  let cached = 0;
  if ("prompt_tokens" in usage) {
    input = num(usage.prompt_tokens);
    cached = num(usage.prompt_tokens_details?.cached_tokens);
  } else {
    cached = num(usage.cache_read_input_tokens);
    input = num(usage.input_tokens) + cached + num(usage.cache_creation_input_tokens);
  }
  const output = "completion_tokens" in usage
    ? num(usage.completion_tokens)
    : num(usage.output_tokens);
  // A stream repeats usage (message_start, message_delta, final chunk);
  // the largest value seen is the total for the response.
  acc.input = Math.max(acc.input, input);
  acc.output = Math.max(acc.output, output);
  acc.cached = Math.max(acc.cached, cached);
  return acc;
}

/** Usage of one response body, SSE or plain JSON. */
export function usageOfBody(text) {
  const acc = { input: 0, output: 0, cached: 0 };
  const visit = (value) => {
    if (!value || typeof value !== "object") return;
    foldUsage(acc, value.usage);
    foldUsage(acc, value.message?.usage);
  };
  const trimmed = text.trimStart();
  if (trimmed.startsWith("{")) {
    try {
      visit(JSON.parse(trimmed));
    } catch {
      // Not JSON after all; fall through to the SSE scan.
    }
  }
  for (const line of text.split("\n")) {
    if (!line.startsWith("data:")) continue;
    const payload = line.slice(5).trim();
    if (!payload.startsWith("{") || !payload.includes("usage")) continue;
    try {
      visit(JSON.parse(payload));
    } catch {
      // A partial or non-JSON data line carries no usage.
    }
  }
  return acc;
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

async function forward(req, res, tag, rest) {
  const row = entry(tag);
  const body = await readBody(req);
  const headers = {};
  for (const [name, value] of Object.entries(req.headers)) {
    if (!HOP_BY_HOP.has(name) && typeof value === "string") headers[name] = value;
  }
  if (API_KEY) {
    if ("x-api-key" in headers) headers["x-api-key"] = API_KEY;
    else headers["authorization"] = `Bearer ${API_KEY}`;
  }
  let model = "";
  if (body.length) {
    try {
      model = String(JSON.parse(body.toString("utf8")).model || "");
    } catch {
      // Non-JSON request bodies are forwarded untouched and not attributed.
    }
  }
  let upstream;
  try {
    upstream = await fetch(`${UPSTREAM}/${rest}`, {
      method: req.method,
      headers,
      body: body.length ? body : undefined,
      redirect: "manual",
    });
  } catch (err) {
    row.errors += 1;
    persist();
    res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { message: `relay: upstream unreachable: ${err}` } }));
    return;
  }
  const outHeaders = {};
  upstream.headers.forEach((value, name) => {
    if (!HOP_BY_HOP.has(name) && name !== "content-encoding") outHeaders[name] = value;
  });
  res.writeHead(upstream.status, outHeaders);
  const decoder = new TextDecoder();
  let text = "";
  if (upstream.body) {
    for await (const chunk of upstream.body) {
      res.write(chunk);
      text += decoder.decode(chunk, { stream: true });
    }
  }
  res.end();
  if (model) {
    row.requests += 1;
    row.models[model] = (row.models[model] || 0) + 1;
    if (upstream.status >= 400) row.errors += 1;
    const usage = usageOfBody(text);
    row.input += usage.input;
    row.output += usage.output;
    row.cached += usage.cached;
    persist();
  }
}

export const server = http.createServer((req, res) => {
  const url = req.url || "/";
  if (url === "/stats") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(Object.fromEntries(stats)));
    return;
  }
  const one = /^\/stats\/([^/]+)$/.exec(url);
  if (one) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(stats.get(one[1]) || entry(one[1])));
    return;
  }
  const match = /^\/t\/([^/]+)\/(.*)$/.exec(url);
  if (!match) {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { message: "relay: use /t/<tag>/<provider path>" } }));
    return;
  }
  forward(req, res, match[1], match[2]).catch((err) => {
    if (!res.headersSent) res.writeHead(500);
    res.end(String(err));
  });
});

if (import.meta.main ?? true) {
  server.listen(PORT, "0.0.0.0", () => {
    console.log(`relay: listening on :${PORT}, upstream ${UPSTREAM}`);
  });
}
