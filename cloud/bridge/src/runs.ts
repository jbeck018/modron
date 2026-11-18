// Cloud runs (PLAN 10.10): one `modron serve` container per run. The bridge
// builds the per-run environment at wake time (the account's git token is
// unsealed here and goes only to the container worker over https) and calls
// the container worker's /wake and /stop with WAKE_TOKEN. Nothing here is
// browser-facing: run rows never carry a token.

import { REPO_RE, isLoopback } from "./github";

export const RUN_ID_RE = /^[a-z0-9-]{1,64}$/;
export const RUN_STATES = ["starting", "running", "sleeping", "stopped", "failed"] as const;
// Git refs and handoff refs: a conservative character set, no leading dash,
// no "..", at most 200 characters. The empty string means "not given".
const REF_RE = /^[A-Za-z0-9._/-]{1,200}$/;
const CONTAINER_CALL_MS = 10 * 1000;
// A status poll rides on the run list, so it gives up sooner than a wake.
const STATUS_CALL_MS = 4 * 1000;

export interface RunsEnv {
  MODRON_TEST_HOOKS?: string;
  CONTAINER_URL?: string;
  WAKE_TOKEN?: string;
  HOST_TOKEN?: string;
}

export interface RunRow {
  id: string;
  repo: string;
  account_id: string;
  branch: string;
  state: string;
  reason: string | null;
  created_at: number;
  session_id: string | null;
}

// The container worker's base URL, or null when it is not usable. https only;
// plain http is accepted only for a loopback address under MODRON_TEST_HOOKS.
export function containerBase(env: RunsEnv): string | null {
  if (!env.CONTAINER_URL || !env.WAKE_TOKEN || !env.HOST_TOKEN) return null;
  let url: URL;
  try { url = new URL(env.CONTAINER_URL); } catch { return null; }
  if (url.search !== "" || url.hash !== "") return null;
  const local = env.MODRON_TEST_HOOKS !== undefined && url.protocol === "http:" && isLoopback(url);
  if (url.protocol !== "https:" && !local) return null;
  return url.href.replace(/\/$/, "");
}

// owner/name, the same shape the container entrypoint accepts.
export function validRepo(value: unknown): value is string {
  return typeof value === "string" && REPO_RE.test(value);
}

// "" (not given) or a ref from REF_RE that is not an option and has no "..".
export function validRef(value: unknown): value is string {
  if (typeof value !== "string") return false;
  if (value === "") return true;
  return REF_RE.test(value) && !value.startsWith("-") && !value.includes("..");
}

// The per-run environment the container worker takes (cloud/container
// src/worker.ts parseRunEnv). The field names are fixed by PLAN 10.10.
export function runEnvironment(
  runId: string,
  bridgeUrl: string,
  hostToken: string,
  row: { repo: string; branch: string },
  gitToken: string,
  handoffRef: string,
): Record<string, string> {
  return {
    run_id: runId,
    bridge_url: bridgeUrl,
    host_token: hostToken,
    repo: row.repo,
    branch: row.branch,
    git_token: gitToken,
    handoff_ref: handoffRef,
  };
}

// POST {container}{path} with the bearer and a JSON body. Ok on a 2xx answer.
// The reason is short and carries the status only, never the body.
export async function containerPost(
  env: RunsEnv,
  base: string,
  path: string,
  body: unknown,
): Promise<{ ok: true } | { ok: false; reason: string }> {
  let res: Response;
  try {
    res = await fetch(base + path, {
      method: "POST",
      headers: {
        authorization: "Bearer " + (env.WAKE_TOKEN ?? ""),
        "content-type": "application/json",
        "user-agent": "modron-bridge",
      },
      body: JSON.stringify(body),
      redirect: "manual",
      signal: AbortSignal.timeout(CONTAINER_CALL_MS),
    });
  } catch {
    return { ok: false, reason: "container did not answer" };
  }
  if (res.status >= 200 && res.status < 300) return { ok: true };
  return { ok: false, reason: `container answered status ${res.status}` };
}

// POST {container}/status with the bearer and { run_id }: the container's
// running flag. null when the container worker did not give a usable answer,
// so the caller keeps the stored state rather than guessing.
export async function containerRunning(env: RunsEnv, base: string, runId: string): Promise<boolean | null> {
  let res: Response;
  try {
    res = await fetch(base + "/status", {
      method: "POST",
      headers: {
        authorization: "Bearer " + (env.WAKE_TOKEN ?? ""),
        "content-type": "application/json",
        "user-agent": "modron-bridge",
      },
      body: JSON.stringify({ run_id: runId }),
      redirect: "manual",
      signal: AbortSignal.timeout(STATUS_CALL_MS),
    });
  } catch {
    return null;
  }
  if (res.status !== 200) return null;
  try {
    const body = (await res.json()) as { running?: unknown };
    return typeof body.running === "boolean" ? body.running : null;
  } catch {
    return null;
  }
}
