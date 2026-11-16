// Linked GitHub accounts (PLAN 10.10). Pure helpers: no Durable Object and no
// Cloudflare import, so the routes in src/index.ts stay thin. Outbound calls
// go to one origin only: https://api.github.com. The only override is
// GITHUB_API_ORIGIN, honoured under MODRON_TEST_HOOKS and only for a loopback
// address (test/cloud.mjs points it at a local fake GitHub).
//
// Nothing here returns or logs a token, and nothing returns GitHub's response
// body beyond the fields listed in PLAN 10.10 (login, and the repo fields).

import type { Checked } from "./vault";

export const GITHUB_ORIGIN = "https://api.github.com";
export const ACCOUNT_LIMIT = 10;
export const ACCOUNT_ID_RE = /^[0-9a-f]{32}$/;
export const REPO_RE = /^[A-Za-z0-9._-]{1,100}\/[A-Za-z0-9._-]{1,100}$/;
const LOGIN_RE = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/;
const TOKEN_MAX_BYTES = 512;
const CONTROL_RE = /[\u0000-\u001f\u007f\s]/;
const REPOS_PER_PAGE = 50;
const SCAN_PER_PAGE = 100;
const SCAN_PAGES = 3;
const TIMEOUT_MS = 10 * 1000;

export interface GithubEnv {
  MODRON_TEST_HOOKS?: string;
  GITHUB_API_ORIGIN?: string;
}

export interface RepoRow {
  full_name: string;
  private: boolean;
  default_branch: string;
  pushed_at: string | null;
}

// A GitHub token as the owner pastes it: a non-empty token of visible ASCII or
// UTF-8 without whitespace or controls, at most 512 bytes. The message never
// repeats the value.
export function checkGithubToken(value: unknown): Checked<string> {
  if (typeof value !== "string") return { ok: false, message: "token must be a string" };
  if (value.length === 0) return { ok: false, message: "token must not be empty" };
  if (new TextEncoder().encode(value).length > TOKEN_MAX_BYTES) {
    return { ok: false, message: `token must be at most ${TOKEN_MAX_BYTES} bytes` };
  }
  if (CONTROL_RE.test(value)) return { ok: false, message: "token has whitespace or control characters" };
  return { ok: true, value };
}

// The GitHub API origin for this request: the fixed one, unless the test hook
// points it at a loopback fake.
export function githubOrigin(env: GithubEnv): string {
  const override = env.MODRON_TEST_HOOKS ? env.GITHUB_API_ORIGIN : undefined;
  if (override === undefined) return GITHUB_ORIGIN;
  let url: URL;
  try { url = new URL(override); } catch { return GITHUB_ORIGIN; }
  if (!isLoopback(url) && url.protocol !== "https:") return GITHUB_ORIGIN;
  return url.origin;
}

export function isLoopback(url: URL): boolean {
  return url.hostname === "127.0.0.1" || url.hostname === "localhost" || url.hostname === "[::1]";
}

// GET one GitHub API path with the account's token. Returns the status and,
// for a 2xx answer only, the parsed JSON body (the caller reads fields from it
// and nothing else). Null when GitHub did not answer in time.
export async function githubGet(
  env: GithubEnv,
  token: string,
  path: string,
): Promise<{ status: number; body: unknown } | null> {
  let res: Response;
  try {
    res = await fetch(githubOrigin(env) + path, {
      method: "GET",
      headers: {
        authorization: "Bearer " + token,
        accept: "application/vnd.github+json",
        "user-agent": "modron-bridge",
      },
      redirect: "manual",
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
  } catch {
    return null;
  }
  if (!res.ok) return { status: res.status, body: null };
  try {
    return { status: res.status, body: await res.json() };
  } catch {
    return { status: res.status, body: null };
  }
}

// The login of GET /user, or null when the answer has no usable login.
export function loginOf(body: unknown): string | null {
  if (body === null || typeof body !== "object") return null;
  const login = (body as { login?: unknown }).login;
  return typeof login === "string" && LOGIN_RE.test(login) ? login : null;
}

// The fields PLAN 10.10 lists, from GET /user/repos entries. Entries without a
// usable full_name are dropped.
export function mapRepos(body: unknown): RepoRow[] | null {
  if (!Array.isArray(body)) return null;
  const out: RepoRow[] = [];
  for (const item of body) {
    if (item === null || typeof item !== "object") continue;
    const r = item as Record<string, unknown>;
    if (typeof r.full_name !== "string" || !REPO_RE.test(r.full_name)) continue;
    out.push({
      full_name: r.full_name,
      private: r.private === true,
      default_branch: typeof r.default_branch === "string" ? r.default_branch : "",
      pushed_at: typeof r.pushed_at === "string" ? r.pushed_at : null,
    });
  }
  return out;
}

// One page of the account's repositories. Without a filter: one GitHub page
// of 50 (sorted by pushed). With `q`: GitHub pages 1 to 3 at 100 each are
// read and filtered here (case-insensitive substring of owner/name), then cut
// into pages of 50. A filter therefore sees at most 300 repositories (v1).
export async function repoPage(
  env: GithubEnv,
  token: string,
  q: string,
  page: number,
): Promise<{ repos: RepoRow[]; next_page: number | null } | "unavailable"> {
  if (q === "") {
    const got = await githubGet(env, token, `/user/repos?sort=pushed&per_page=${REPOS_PER_PAGE}&page=${page}`);
    if (got === null || got.status !== 200) return "unavailable";
    const repos = mapRepos(got.body);
    if (repos === null) return "unavailable";
    return { repos, next_page: repos.length === REPOS_PER_PAGE ? page + 1 : null };
  }
  const scanned: RepoRow[] = [];
  for (let p = 1; p <= SCAN_PAGES; p++) {
    const got = await githubGet(env, token, `/user/repos?sort=pushed&per_page=${SCAN_PER_PAGE}&page=${p}`);
    if (got === null || got.status !== 200) return "unavailable";
    const repos = mapRepos(got.body);
    if (repos === null) return "unavailable";
    scanned.push(...repos);
    if (repos.length < SCAN_PER_PAGE) break;
  }
  const needle = q.toLowerCase();
  const matches = scanned.filter((r) => r.full_name.toLowerCase().includes(needle));
  const start = (page - 1) * REPOS_PER_PAGE;
  return {
    repos: matches.slice(start, start + REPOS_PER_PAGE),
    next_page: matches.length > start + REPOS_PER_PAGE ? page + 1 : null,
  };
}
