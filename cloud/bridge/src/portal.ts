// Portal helpers shared by the bridge router (src/index.ts) and the
// Directory object (src/directory.ts). Nothing here touches storage.
// Secrets: OWNER_TOKEN (owner sign-in, invite signing key) and HOST_TOKEN
// (host registration and the agent tunnel). Neither is ever logged.

// Share keys (agent and viewer) and every key path segment.
export const KEY_RE = /^[a-z0-9-]{1,64}$/;
export const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export const INVITE_ID_RE = /^[0-9a-f]{32}$/;
export const TICKET_RE = /^[0-9a-f]{64}$/;
const INVITE_TOKEN_RE = /^([0-9a-f]{32})\.([0-9a-f]{64})$/;
const CONTROL_RE = /[\u0000-\u001f\u007f]/;

// JSON bodies on the portal routes are capped at 16 KB.
export const JSON_MAX_BYTES = 16 * 1024;
export const OWNER_COOKIE = "modron_owner";
export const OWNER_MAX_AGE_S = 30 * 24 * 3600; // 30 days, also the server-side TTL
export const OWNER_TTL_MS = OWNER_MAX_AGE_S * 1000;
export const TICKET_TTL_MS = 60 * 1000;
export const AUTH_WINDOW_MS = 60 * 1000;
export const AUTH_MAX_ATTEMPTS = 5;
// Hosts re-PUT every 60 s (crates/modron-server/src/rc.rs). A session whose
// last PUT is older than 5 minutes (5 missed heartbeats) is reported offline,
// and a registration older than 30 days is deleted outright.
export const SESSION_STALE_MS = 5 * 60 * 1000;
export const SESSION_GC_MS = 30 * 24 * 3600 * 1000;

const enc = new TextEncoder();
const INVITE_SALT = "modron-portal-v1";
const INVITE_INFO = "modron-invite-sig-v1";

export function toHex(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

// Every random id in the portal comes from here (crypto.getRandomValues).
export function randomHex(bytes: number): string {
  const buf = new Uint8Array(bytes);
  crypto.getRandomValues(buf);
  return toHex(buf);
}

export async function sha256Hex(text: string): Promise<string> {
  return toHex(new Uint8Array(await crypto.subtle.digest("SHA-256", enc.encode(text))));
}

// Constant-time string equality. Compares UTF-8 bytes and always walks the
// longer input, so the timing does not reveal the first mismatch position.
export function constantTimeEquals(a: string, b: string): boolean {
  const x = enc.encode(a);
  const y = enc.encode(b);
  let diff = x.length ^ y.length;
  const n = Math.max(x.length, y.length);
  for (let i = 0; i < n; i++) diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return diff === 0;
}

// --- invite signatures: HMAC-SHA256(invite id), key from OWNER_TOKEN via HKDF ---

async function inviteKey(ownerToken: string): Promise<CryptoKey> {
  const ikm = await crypto.subtle.importKey("raw", enc.encode(ownerToken), "HKDF", false, ["deriveKey"]);
  return crypto.subtle.deriveKey(
    { name: "HKDF", hash: "SHA-256", salt: enc.encode(INVITE_SALT), info: enc.encode(INVITE_INFO) },
    ikm,
    { name: "HMAC", hash: "SHA-256", length: 256 },
    false,
    ["sign"],
  );
}

export async function inviteSignature(ownerToken: string, inviteId: string): Promise<string> {
  const sig = await crypto.subtle.sign("HMAC", await inviteKey(ownerToken), enc.encode(inviteId));
  return toHex(new Uint8Array(sig));
}

// `<id>.<sig>` from an invite URL path, or null when malformed.
export function parseInviteToken(token: string): { id: string; sig: string } | null {
  const m = INVITE_TOKEN_RE.exec(token);
  return m ? { id: m[1], sig: m[2] } : null;
}

// --- owner cookie ---

export function readCookie(request: Request, name: string): string | null {
  const header = request.headers.get("cookie");
  if (!header) return null;
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    if (part.slice(0, eq).trim() === name) return part.slice(eq + 1).trim();
  }
  return null;
}

// Plain http to 127.0.0.1 or localhost (local dev only): no Secure flag.
export function isLocalDev(url: URL): boolean {
  return url.protocol === "http:" && (url.hostname === "127.0.0.1" || url.hostname === "localhost");
}

// value === null clears the cookie.
export function ownerCookieHeader(value: string | null, secure: boolean): string {
  const attrs = ["HttpOnly", ...(secure ? ["Secure"] : []), "SameSite=Strict", "Path=/"];
  return value === null
    ? [`${OWNER_COOKIE}=`, ...attrs, "Max-Age=0"].join("; ")
    : [`${OWNER_COOKIE}=${value}`, ...attrs, `Max-Age=${OWNER_MAX_AGE_S}`].join("; ");
}

// --- bounded JSON bodies with a closed field list ---

export type Check<T> = { ok: true; value: T } | { ok: false; message: string };
const good = <T>(value: T): Check<T> => ({ ok: true, value });
const bad = (message: string): { ok: false; message: string } => ({ ok: false, message });

export type JsonBody =
  | { ok: true; body: Record<string, unknown> }
  | { ok: false; response: Response };

function plain(status: number, message: string): Response {
  return new Response(message, { status, headers: { "cache-control": "no-store" } });
}

// Reads a JSON object of at most 16 KB. Any field not in `allowed` is an
// error: a field that could widen authorization must never be ignored.
export async function readJsonObject(request: Request, allowed: readonly string[]): Promise<JsonBody> {
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    const n = Number(declared);
    if (!Number.isInteger(n) || n < 0) return { ok: false, response: plain(400, "bad content-length") };
    if (n > JSON_MAX_BYTES) return { ok: false, response: plain(413, "body too large (max 16KB)") };
  }
  const raw = await request.text();
  if (enc.encode(raw).length > JSON_MAX_BYTES) return { ok: false, response: plain(413, "body too large (max 16KB)") };
  let parsed: unknown;
  try { parsed = JSON.parse(raw); } catch { return { ok: false, response: plain(400, "bad json") }; }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { ok: false, response: plain(400, "expected a JSON object") };
  }
  const body = parsed as Record<string, unknown>;
  for (const field of Object.keys(body)) {
    if (!allowed.includes(field)) return { ok: false, response: plain(400, "unknown field") };
  }
  return { ok: true, body };
}

function textField(body: Record<string, unknown>, field: string, min: number, max: number): Check<string> {
  const v = body[field];
  if (typeof v !== "string") return bad(`${field} must be a string`);
  const t = v.trim();
  if (t.length < min || t.length > max) return bad(`${field} must be ${min}-${max} characters`);
  if (CONTROL_RE.test(t)) return bad(`${field} has control characters`);
  return good(t);
}

// --- host registration (PUT /api/host/sessions/<id>) ---

export interface HostSessionInput {
  title: string;
  host_label: string;
  host_kind: "local" | "cloud";
  viewer_key: string;
  state: string;
}

export function parseHostSession(body: Record<string, unknown>): Check<HostSessionInput> {
  const title = textField(body, "title", 1, 200);
  if (!title.ok) return title;
  const host_label = textField(body, "host_label", 1, 100);
  if (!host_label.ok) return host_label;
  const kind = body.host_kind;
  if (kind !== "local" && kind !== "cloud") return bad("host_kind must be local or cloud");
  const viewer_key = body.viewer_key;
  if (typeof viewer_key !== "string" || !KEY_RE.test(viewer_key)) return bad("viewer_key is not a valid key");
  const state = body.state;
  if (typeof state !== "string" || !/^[a-z][a-z_]{0,31}$/.test(state)) return bad("state must be a short lowercase token");
  return good({
    title: title.value,
    host_label: host_label.value,
    host_kind: kind,
    viewer_key,
    state,
  });
}

// --- session rows as the owner sees them (GET /api/sessions) ---

// The stored row (see directory.ts SessionRow), with updated_at in ms.
export interface StoredSession {
  id: string;
  title: string;
  host_label: string;
  host_kind: string;
  viewer_key: string;
  state: string;
  updated_at: number;
}

export interface SessionView {
  id: string;
  title: string;
  host_label: string;
  host_kind: string;
  viewer_key: string;
  // "offline" when the last heartbeat is stale; the stored state otherwise.
  state: string;
  updated_at: string;
  online: boolean;
  // Path the owner's browser opens for this session (same origin, so the
  // owner cookie rides along on the viewer websocket).
  open_url: string;
}

// `wsOrigin` is the websocket origin of the request, e.g. wss://bridge.example
// or ws://127.0.0.1:8787. The inner url is URL-encoded in the query string.
export function sessionOpenUrl(wsOrigin: string, viewerKey: string): string {
  return "/app/?url=" + encodeURIComponent(wsOrigin + "/ws/" + viewerKey);
}

export function sessionView(row: StoredSession, now: number, wsOrigin: string): SessionView {
  const online = now - row.updated_at <= SESSION_STALE_MS;
  return {
    id: row.id,
    title: row.title,
    host_label: row.host_label,
    host_kind: row.host_kind,
    viewer_key: row.viewer_key,
    state: online ? row.state : "offline",
    updated_at: new Date(row.updated_at).toISOString(),
    online,
    open_url: sessionOpenUrl(wsOrigin, row.viewer_key),
  };
}

// --- invites (POST /api/invites) ---

export interface InviteInput {
  session_id: string;
  role: "viewer" | "approver";
  ttl_ms: number;
}

// `hooks` is true only under MODRON_TEST_HOOKS, which adds ttl_seconds
// (1..3600) so the expiry test does not wait an hour. The router already
// rejected ttl_seconds when hooks are off.
export function parseInvite(body: Record<string, unknown>, hooks: boolean): Check<InviteInput> {
  const sid = body.session_id;
  if (typeof sid !== "string" || !UUID_RE.test(sid)) return bad("session_id must be a uuid");
  const role = body.role;
  if (role !== "viewer" && role !== "approver") return bad("role must be viewer or approver");
  const hasHours = "ttl_hours" in body;
  const hasSeconds = hooks && "ttl_seconds" in body;
  if (hasHours === hasSeconds) return bad("give exactly one of ttl_hours or ttl_seconds");
  let ttl_ms: number;
  if (hasSeconds) {
    const s = body.ttl_seconds;
    if (typeof s !== "number" || !Number.isInteger(s) || s < 1 || s > 3600) return bad("ttl_seconds must be 1-3600");
    ttl_ms = s * 1000;
  } else {
    const h = body.ttl_hours;
    if (typeof h !== "number" || !Number.isInteger(h) || h < 1 || h > 720) return bad("ttl_hours must be 1-720");
    ttl_ms = h * 3600 * 1000;
  }
  return good({ session_id: sid.toLowerCase(), role, ttl_ms });
}

// --- viewer frames from a role-tagged socket ---

export const PERMITTED_FRAMES: Record<"viewer" | "approver", ReadonlySet<string>> = {
  viewer: new Set(["hello", "attach", "detach", "ping", "sync"]),
  approver: new Set(["hello", "attach", "detach", "ping", "sync", "approve", "decide_plan"]),
};

export type ViewerVerdict =
  | { verdict: "forward" }
  | { verdict: "malformed" }
  | { verdict: "forbidden"; id?: number };

// Malformed JSON (or a non-object) is dropped silently. A well-formed frame
// whose `type` the role may not send is answered with a forbidden error
// (echoing the request `id` when it had one) and never forwarded.
export function judgeViewerFrame(role: "viewer" | "approver", message: string): ViewerVerdict {
  let parsed: unknown;
  try { parsed = JSON.parse(message); } catch { return { verdict: "malformed" }; }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) return { verdict: "malformed" };
  const frame = parsed as { type?: unknown; id?: unknown };
  if (typeof frame.type === "string" && PERMITTED_FRAMES[role].has(frame.type)) return { verdict: "forward" };
  const id = typeof frame.id === "number" && Number.isSafeInteger(frame.id) && frame.id >= 0 ? frame.id : undefined;
  return { verdict: "forbidden", id };
}

// The protocol's Error payload (crates/modron-protocol Payload::Error with
// kind Forbidden), plus the request id when there was one.
export function forbiddenFrame(id?: number): string {
  const base = { type: "error", message: "not permitted for this invite", kind: "forbidden" };
  return JSON.stringify(id === undefined ? base : { ...base, id });
}
