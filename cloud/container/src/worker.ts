// modron cloud container worker. POST /wake?run=<id> with
// `Authorization: Bearer <WAKE_TOKEN>` starts the container for that run
// (one ModronAgent Durable Object per run). Nothing else is forwarded: the
// container dials out to the bridge itself, and it sleeps after
// SLEEP_AFTER_MS without requests (idle = sleep; the next wake restores it).
//
// Secrets reach the container only as its start environment (below). They
// are never logged, returned, or written into the image.
//
// UNVERIFIED: the Containers API calls (ctx.container, start, running,
// setInactivityTimeout) follow the Cloudflare Containers docs as summarised
// when this file was written; check them against the docs before a deploy.
import { DurableObject } from "cloudflare:workers";

export interface Env {
  MODRON_AGENT: DurableObjectNamespace;
  WAKE_TOKEN?: string;
  BRIDGE_URL?: string;
  GITHUB_REPO?: string;
  GITHUB_TOKEN?: string;
  MODRON_HOST_TOKEN?: string;
}

// Environment names passed through to the container (all from Worker secrets).
const CONTAINER_ENV_KEYS = [
  "BRIDGE_URL",
  "GITHUB_REPO",
  "GITHUB_TOKEN",
  "MODRON_HOST_TOKEN",
] as const;

const SLEEP_AFTER_MS = 10 * 60 * 1000;
const RUN_ID = /^[a-z0-9-]{1,64}$/;

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

// Constant-time string comparison; false on length mismatch.
function safeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

function authorized(request: Request, token: string | undefined): boolean {
  if (!token) return false; // WAKE_TOKEN unset: wake is disabled
  const header = request.headers.get("authorization") ?? "";
  return safeEqual(header, `Bearer ${token}`);
}

function containerEnv(env: Env): Record<string, string> {
  const out: Record<string, string> = { MODRON_HOST_KIND: "cloud" };
  for (const key of CONTAINER_ENV_KEYS) {
    const value = env[key];
    if (value) out[key] = value;
  }
  return out;
}

export class ModronAgent extends DurableObject<Env> {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/wake") {
      return this.wake();
    }
    if (request.method === "GET" && url.pathname === "/status") {
      return json({ running: this.ctx.container?.running ?? false });
    }
    return new Response("not found", { status: 404 });
  }

  private async wake(): Promise<Response> {
    const container = this.ctx.container;
    if (!container) {
      return json({ error: "containers are not enabled for this worker" }, 500);
    }
    container.setInactivityTimeout(SLEEP_AFTER_MS);
    if (!container.running) {
      await container.start({ env: containerEnv(this.env) });
    }
    return json({ running: true }, 202);
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/wake" && request.method === "POST") {
      if (!env.WAKE_TOKEN) {
        return json({ error: "wake is disabled: WAKE_TOKEN is not set" }, 503);
      }
      if (!authorized(request, env.WAKE_TOKEN)) {
        return json({ error: "unauthorized" }, 401);
      }
      const run = url.searchParams.get("run") ?? "default";
      if (!RUN_ID.test(run)) {
        return json({ error: "run must match [a-z0-9-]{1,64}" }, 400);
      }
      const stub = env.MODRON_AGENT.get(env.MODRON_AGENT.idFromName(run));
      return stub.fetch(new Request("https://agent.internal/wake", { method: "POST" }));
    }
    return new Response("modron cloud container runtime", { status: 404 });
  },
};
