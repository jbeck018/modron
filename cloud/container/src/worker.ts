// modron cloud container worker (PLAN 10.10). The bridge calls
//   POST /wake  with `Authorization: Bearer <WAKE_TOKEN>` and the per-run
//               environment as the JSON body (fields below), and
//   POST /stop  with the same bearer and body { "run_id": "<id>" }, and
//   POST /status with the same bearer and body { "run_id": "<id>" }, which
//               answers { "running": boolean } (PLAN 10.10 "sleeping": the
//               platform stopped the container after its idle timeout).
// One ModronAgent Durable Object (and so one Container) per run id. Wake
// starts the container with the run's environment, or wakes it; it sleeps
// after SLEEP_AFTER_MS without requests. Stop destroys the container.
//
// The worker holds no provider keys, no repository and no git token of its
// own: the only secret it is deployed with is WAKE_TOKEN. The run environment
// is passed to the container as its start environment and is never logged,
// returned, or stored (the Durable Object keeps nothing between requests).
//
// UNVERIFIED: the Containers API calls (ctx.container, start, running,
// setInactivityTimeout, destroy) follow the Cloudflare Containers docs as
// summarised when this file was written; check them against the docs before a
// deploy.
import { DurableObject } from "cloudflare:workers";

export interface Env {
  MODRON_AGENT: DurableObjectNamespace;
  // Wrangler secret (`wrangler secret put WAKE_TOKEN`). Unset: wake and stop
  // answer 503. Must equal the bridge's WAKE_TOKEN secret.
  WAKE_TOKEN?: string;
}

// The per-run environment (the wake body, and the stop body's run_id). Each
// field maps to one variable of the container; the entrypoint reads these.
const RUN_FIELDS: Readonly<Record<string, { env: string; max: number; required: boolean }>> = {
  run_id: { env: "MODRON_RUN_ID", max: 64, required: true },
  bridge_url: { env: "MODRON_BRIDGE_URL", max: 2048, required: true },
  host_token: { env: "MODRON_HOST_TOKEN", max: 512, required: true },
  repo: { env: "MODRON_REPO", max: 201, required: true },
  branch: { env: "MODRON_BRANCH", max: 200, required: false },
  git_token: { env: "MODRON_GIT_TOKEN", max: 512, required: true },
  handoff_ref: { env: "MODRON_HANDOFF_REF", max: 200, required: false },
};

const RUN_ID = /^[a-z0-9-]{1,64}$/;
const REPO = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;
const REF = /^[A-Za-z0-9._/-]{1,200}$/;
const SECRET_CHARS = /[\u0000- \u007f]/; // no whitespace or control characters
const BODY_MAX_BYTES = 16 * 1024;
const SLEEP_AFTER_MS = 10 * 60 * 1000;

export type Checked<T> = { ok: true; value: T } | { ok: false; message: string };

// Validates the wake body. Unknown fields are refused; every value is a
// string within its size. Returns the container environment (empty optional
// fields are left out). The message never repeats a value.
export function parseRunEnv(body: unknown): Checked<Record<string, string>> {
  if (body === null || typeof body !== "object" || Array.isArray(body)) {
    return { ok: false, message: "body must be a JSON object" };
  }
  const input = body as Record<string, unknown>;
  for (const name of Object.keys(input)) {
    if (!Object.prototype.hasOwnProperty.call(RUN_FIELDS, name)) return { ok: false, message: `unknown field ${name}` };
  }
  const out: Record<string, string> = {};
  for (const [name, spec] of Object.entries(RUN_FIELDS)) {
    const value = input[name];
    if (value === undefined) {
      if (spec.required) return { ok: false, message: `${name} is required` };
      continue;
    }
    if (typeof value !== "string") return { ok: false, message: `${name} must be a string` };
    if (value.length > spec.max) return { ok: false, message: `${name} is longer than ${spec.max} characters` };
    if (value === "") {
      if (spec.required) return { ok: false, message: `${name} must not be empty` };
      continue;
    }
    const problem = fieldProblem(name, value);
    if (problem !== null) return { ok: false, message: problem };
    out[spec.env] = value;
  }
  return { ok: true, value: out };
}

// Why one non-empty field value is refused, or null when it is fine.
function fieldProblem(name: string, value: string): string | null {
  switch (name) {
    case "run_id":
      return RUN_ID.test(value) ? null : "run_id must match [a-z0-9-]{1,64}";
    case "bridge_url": {
      let url: URL;
      try { url = new URL(value); } catch { return "bridge_url is not a URL"; }
      return (url.protocol === "https:" || url.protocol === "wss:") && url.username === "" && url.password === ""
        ? null
        : "bridge_url must be https or wss";
    }
    case "repo":
      return REPO.test(value) ? null : "repo must look like owner/name";
    case "branch":
    case "handoff_ref":
      return REF.test(value) && !value.startsWith("-") && !value.includes("..") ? null : `${name} is not a valid git ref`;
    default:
      return SECRET_CHARS.test(value) ? `${name} has whitespace or control characters` : null;
  }
}

// Constant-time string comparison; false on length mismatch.
function safeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

function authorized(request: Request, token: string | undefined): boolean {
  if (!token) return false; // WAKE_TOKEN unset: wake and stop are disabled
  const header = request.headers.get("authorization") ?? "";
  return safeEqual(header, `Bearer ${token}`);
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

// Reads a JSON object body of at most BODY_MAX_BYTES.
async function readBody(request: Request): Promise<Checked<unknown>> {
  const declared = request.headers.get("content-length");
  if (declared !== null && Number(declared) > BODY_MAX_BYTES) return { ok: false, message: "body too large" };
  const raw = await request.text();
  if (new TextEncoder().encode(raw).length > BODY_MAX_BYTES) return { ok: false, message: "body too large" };
  try {
    return { ok: true, value: JSON.parse(raw) as unknown };
  } catch {
    return { ok: false, message: "body is not JSON" };
  }
}

export class ModronAgent extends DurableObject<Env> {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/wake") {
      const env = (await request.json()) as Record<string, string>;
      return this.wake(env);
    }
    if (request.method === "POST" && url.pathname === "/stop") {
      return this.stop();
    }
    if (request.method === "POST" && url.pathname === "/status") {
      // No container object (containers not enabled) reads as not running.
      return json({ running: this.ctx.container?.running ?? false });
    }
    return new Response("not found", { status: 404 });
  }

  // Starts the container with the run's environment, or leaves a running one
  // alone (its environment is the one it started with).
  private async wake(runEnv: Record<string, string>): Promise<Response> {
    const container = this.ctx.container;
    if (!container) {
      return json({ error: "containers are not enabled for this worker" }, 500);
    }
    container.setInactivityTimeout(SLEEP_AFTER_MS);
    if (!container.running) {
      await container.start({ env: { ...runEnv, MODRON_HOST_KIND: "cloud" } });
    }
    return json({ running: true }, 202);
  }

  private async stop(): Promise<Response> {
    const container = this.ctx.container;
    if (!container) {
      return json({ error: "containers are not enabled for this worker" }, 500);
    }
    if (container.running) await container.destroy();
    return json({ running: false }, 200);
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (request.method !== "POST" || !["/wake", "/stop", "/status"].includes(url.pathname)) {
      return new Response("modron cloud container runtime", { status: 404 });
    }
    if (!env.WAKE_TOKEN) {
      return json({ error: "wake is disabled: WAKE_TOKEN is not set" }, 503);
    }
    if (!authorized(request, env.WAKE_TOKEN)) {
      return json({ error: "unauthorized" }, 401);
    }
    const read = await readBody(request);
    if (!read.ok) return json({ error: read.message }, read.message === "body too large" ? 413 : 400);
    let runId: string;
    let containerEnv: Record<string, string> = {};
    if (url.pathname === "/wake") {
      const parsed = parseRunEnv(read.value);
      if (!parsed.ok) return json({ error: parsed.message }, 400);
      containerEnv = parsed.value;
      runId = parsed.value.MODRON_RUN_ID;
    } else {
      // /stop and /status: the run id names the Durable Object.
      const id = read.value !== null && typeof read.value === "object" ? (read.value as { run_id?: unknown }).run_id : undefined;
      if (typeof id !== "string" || !RUN_ID.test(id)) return json({ error: "run_id must match [a-z0-9-]{1,64}" }, 400);
      runId = id;
    }
    const stub = env.MODRON_AGENT.get(env.MODRON_AGENT.idFromName(runId));
    return stub.fetch(new Request("https://agent.internal" + url.pathname, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(containerEnv),
    }));
  },
};
