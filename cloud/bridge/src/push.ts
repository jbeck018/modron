// Web Push for the portal (cloud/bridge/README.md "Web push"): VAPID
// (RFC 8292, ES256 JWT) and message encryption (RFC 8291, aes128gcm, RFC 8188
// framing), both on WebCrypto. Nothing here touches storage or the network
// except sendPush. This file imports nothing, so test/push.mjs can load it
// directly under Node's type stripping.
// Secrets (Worker): VAPID_PUBLIC_KEY, VAPID_PRIVATE_KEY (base64url, the
// uncompressed P-256 point and the 32-byte scalar), VAPID_SUBJECT (mailto: or
// https:). Never logged.

const enc = new TextEncoder();

// Push services a subscription may name. Anything else is refused at
// subscribe time, so the Worker cannot be used to reach arbitrary hosts.
const PUSH_HOSTS = new Set(["fcm.googleapis.com", "updates.push.services.mozilla.com"]);
const PUSH_HOST_SUFFIXES = [".push.apple.com", ".notify.windows.com"];
const MAX_ENDPOINT = 2048;

export const NOTIFY_KINDS = new Set(["approval", "done", "failed", "plan", "wake", "paused"]);
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const NOTIFY_MAX_CHARS = 1024;

export function b64urlEncode(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

// Throws on anything that is not unpadded base64url.
export function b64urlDecode(text: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]+$/.test(text) || text.length % 4 === 1) throw new Error("bad base64url");
  const padded = text.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(text.length / 4) * 4, "=");
  return Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) { out.set(p, at); at += p.length; }
  return out;
}

// --- endpoint and subscription validation ---

// True when `endpoint` is an https URL on a known push service (or on the
// test origin, which the caller passes only under MODRON_TEST_HOOKS).
export function pushEndpointAllowed(endpoint: string, testOrigin?: string): boolean {
  if (endpoint.length > MAX_ENDPOINT) return false;
  let url: URL;
  try { url = new URL(endpoint); } catch { return false; }
  if (url.username !== "" || url.password !== "") return false;
  if (testOrigin && url.origin === testOrigin) return true;
  if (url.protocol !== "https:" || url.port !== "") return false;
  const host = url.hostname.toLowerCase();
  if (PUSH_HOSTS.has(host)) return true;
  return PUSH_HOST_SUFFIXES.some((suffix) => host.endsWith(suffix) && host.length > suffix.length);
}

// A subscription's browser keys: p256dh is the 65-byte uncompressed P-256
// point, auth the 16-byte secret (both base64url).
export function validSubscriptionKeys(p256dh: string, auth: string): boolean {
  try {
    return b64urlDecode(p256dh).length === 65 && b64urlDecode(p256dh)[0] === 4 && b64urlDecode(auth).length === 16;
  } catch {
    return false;
  }
}

// --- VAPID (RFC 8292) ---

export interface VapidKeys {
  publicKey: string; // base64url, 65 bytes
  privateKey: CryptoKey;
  subject: string;
}

export async function importVapid(publicKey: string, privateScalar: string, subject: string): Promise<VapidKeys> {
  const pub = b64urlDecode(publicKey);
  if (pub.length !== 65 || pub[0] !== 4) throw new Error("VAPID_PUBLIC_KEY is not an uncompressed P-256 point");
  const d = b64urlDecode(privateScalar);
  if (d.length !== 32) throw new Error("VAPID_PRIVATE_KEY is not a 32-byte scalar");
  const privateKey = await crypto.subtle.importKey(
    "jwk",
    { kty: "EC", crv: "P-256", x: b64urlEncode(pub.slice(1, 33)), y: b64urlEncode(pub.slice(33, 65)), d: b64urlEncode(d), ext: true, key_ops: ["sign"] },
    { name: "ECDSA", namedCurve: "P-256" },
    false,
    ["sign"],
  );
  return { publicKey: b64urlEncode(pub), privateKey, subject };
}

// The `t=` JWT for one push service origin, valid for 12 hours.
export async function vapidToken(keys: VapidKeys, audience: string, nowMs: number): Promise<string> {
  const header = b64urlEncode(enc.encode(JSON.stringify({ typ: "JWT", alg: "ES256" })));
  const claims = b64urlEncode(enc.encode(JSON.stringify({
    aud: audience,
    exp: Math.floor(nowMs / 1000) + 12 * 3600,
    sub: keys.subject,
  })));
  const input = header + "." + claims;
  const sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, keys.privateKey, enc.encode(input));
  // WebCrypto returns the raw r || s signature, which is the JWS encoding.
  return input + "." + b64urlEncode(new Uint8Array(sig));
}

// --- payload encryption (RFC 8291 over RFC 8188 aes128gcm) ---

async function hkdf(salt: Uint8Array, ikm: Uint8Array, info: Uint8Array, length: number): Promise<Uint8Array> {
  const key = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
  const bits = await crypto.subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt, info }, key, length * 8);
  return new Uint8Array(bits);
}

export interface EphemeralSender {
  publicKey: Uint8Array; // 65-byte uncompressed point, sent in the header
  privateKey: CryptoKey;
  salt: Uint8Array; // 16 random bytes
}

// Encrypts `plaintext` for one subscription. `sender` is injectable so the
// RFC 8291 section 5 vector can be reproduced exactly; production passes
// nothing and gets a fresh key pair and salt.
export async function encryptPayload(
  plaintext: Uint8Array,
  uaPublic: Uint8Array,
  authSecret: Uint8Array,
  sender?: EphemeralSender,
): Promise<Uint8Array> {
  let as = sender;
  if (!as) {
    const pair = (await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"])) as CryptoKeyPair;
    as = {
      publicKey: new Uint8Array((await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer),
      privateKey: pair.privateKey,
      salt: crypto.getRandomValues(new Uint8Array(16)),
    };
  }
  const uaKey = await crypto.subtle.importKey("raw", uaPublic, { name: "ECDH", namedCurve: "P-256" }, false, []);
  // The runtime reads `public`; the bundled workers-types spell it `$public`.
  const ecdhParams = { name: "ECDH", public: uaKey } as unknown as SubtleCryptoDeriveKeyAlgorithm;
  const ecdhSecret = new Uint8Array(await crypto.subtle.deriveBits(ecdhParams, as.privateKey, 256));
  const keyInfo = concat(enc.encode("WebPush: info\0"), uaPublic, as.publicKey);
  const ikm = await hkdf(authSecret, ecdhSecret, keyInfo, 32);
  const cek = await hkdf(as.salt, ikm, enc.encode("Content-Encoding: aes128gcm\0"), 16);
  const nonce = await hkdf(as.salt, ikm, enc.encode("Content-Encoding: nonce\0"), 12);
  const aes = await crypto.subtle.importKey("raw", cek, { name: "AES-GCM" }, false, ["encrypt"]);
  // One record: the plaintext, the final-record delimiter 0x02, no padding.
  const sealed = new Uint8Array(await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: nonce },
    aes,
    concat(plaintext, Uint8Array.of(2)),
  ));
  const rs = new Uint8Array(4);
  new DataView(rs.buffer).setUint32(0, 4096);
  return concat(as.salt, rs, Uint8Array.of(as.publicKey.length), as.publicKey, sealed);
}

// --- sending ---

// POSTs one encrypted message. Returns the push service's status (201 on
// success; 404 or 410 means the subscription is gone). Never logs the
// endpoint, the keys or the response body.
export async function sendPush(
  endpoint: string,
  sub: { p256dh: string; auth: string },
  payload: string,
  keys: VapidKeys,
  nowMs: number,
): Promise<number> {
  const body = await encryptPayload(enc.encode(payload), b64urlDecode(sub.p256dh), b64urlDecode(sub.auth));
  const token = await vapidToken(keys, new URL(endpoint).origin, nowMs);
  const res = await fetch(endpoint, {
    method: "POST",
    headers: {
      "content-encoding": "aes128gcm",
      "content-type": "application/octet-stream",
      ttl: "300",
      urgency: "high",
      authorization: `vapid t=${token}, k=${keys.publicKey}`,
    },
    body,
  });
  await res.body?.cancel();
  return res.status;
}

// --- notify frames (docs/PROTOCOL_V2.md section 6) ---

export interface NotifyFrame {
  session: string;
  kind: string;
  text: string;
}

// The Notify frame the serve sends (text is the server's own wording and is
// forwarded exactly). Null for anything else.
export function parseNotify(message: string): NotifyFrame | null {
  let parsed: unknown;
  try { parsed = JSON.parse(message); } catch { return null; }
  if (parsed === null || typeof parsed !== "object") return null;
  const f = parsed as Record<string, unknown>;
  if (f.type !== "notify") return null;
  if (typeof f.session !== "string" || !UUID_RE.test(f.session)) return null;
  if (typeof f.kind !== "string" || !NOTIFY_KINDS.has(f.kind)) return null;
  if (typeof f.text !== "string" || f.text.length === 0 || f.text.length > NOTIFY_MAX_CHARS) return null;
  return { session: f.session.toLowerCase(), kind: f.kind, text: f.text };
}

// The service worker the web app registers at /app/sw.js (served by the
// Worker, so the bundle needs no extra file). It shows the push and opens
// the session on a tap. No caching and no fetch handler.
export const PUSH_WORKER = `// Modron web push receiver (served by the bridge at /app/sw.js).
self.addEventListener("install", function () { self.skipWaiting(); });
self.addEventListener("activate", function (event) { event.waitUntil(self.clients.claim()); });
self.addEventListener("push", function (event) {
  var data = {};
  try { data = event.data ? event.data.json() : {}; } catch (e) { data = {}; }
  var title = typeof data.title === "string" && data.title ? data.title : "modron";
  var body = typeof data.body === "string" ? data.body : "";
  var url = data.data && typeof data.data.url === "string" ? data.data.url : "/app/";
  event.waitUntil(self.registration.showNotification(title, { body: body, data: { url: url } }));
});
self.addEventListener("notificationclick", function (event) {
  event.notification.close();
  var raw = event.notification.data && event.notification.data.url;
  var target = new URL(typeof raw === "string" ? raw : "/app/", self.location.origin);
  if (target.origin !== self.location.origin) target = new URL("/app/", self.location.origin);
  event.waitUntil(self.clients.matchAll({ type: "window", includeUncontrolled: true }).then(function (clients) {
    for (const client of clients) {
      if (client.url === target.href && "focus" in client) return client.focus();
    }
    return self.clients.openWindow(target.href);
  }));
});
`;
