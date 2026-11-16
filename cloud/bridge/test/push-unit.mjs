// Unit checks for src/push.ts against RFC 8291 (section 5 and appendix A)
// and RFC 8292 (the section 3 example JWT). No Worker runs here:
//   node --experimental-strip-types test/push-unit.mjs   (from cloud/bridge)
// Decryption below uses node:crypto, not the code under test.
import { createDecipheriv, createECDH, createHash, hkdfSync } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const push = await import(join(here, "..", "src", "push.ts"));
const { b64urlDecode, b64urlEncode, encryptPayload, importVapid, parseNotify, pushEndpointAllowed, validSubscriptionKeys, vapidToken } = push;

let failures = 0;
const check = (name, ok, detail = "") => {
  console.log((ok ? "PASS " : "FAIL ") + name + (ok || !detail ? "" : " -- " + detail));
  if (!ok) failures++;
};
const bytesEq = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);
const cat = (...parts) => Buffer.concat(parts.map((p) => Buffer.from(p)));

// RFC 8291 section 5 and appendix A.
const RFC = {
  plaintext: "When I grow up, I want to be a watermelon",
  asPublic: "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
  asPrivate: "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw",
  uaPublic: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
  uaPrivate: "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94",
  salt: "DGv6ra1nlYgDCS1FRnbzlw",
  auth: "BTBZMqHH6r4Tts7J_aSIgg",
  // Section 5 body, the three lines joined.
  body: "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT" +
    "pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
  // Appendix A: the header (86 octets) and the ciphertext (plaintext + 0x02, sealed).
  header: "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
  ciphertext: "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ",
  // RFC 8292 section 3.1 example.
  vapidT: "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.eyJhdWQiOiJodHRwczovL3B1c2guZXhhbXBsZS5uZXQiLCJleHAiOjE0NTM1MjM3NjgsInN1YiI6Im1haWx0bzpwdXNoQGV4YW1wbGUuY29tIn0.i3CYb7t4xfxCDquptFOepC9GAu_HLGkMlMuCGSK2rpiUfnK9ojFwDXb1JrErtmysazNjjvW2L9OkSSHzvoD1oA",
  vapidK: "BA1Hxzyi1RUM1b5wjxsn7nGxAszw2u61m164i3MrAIxHF6YK5h4SDYic-dRuU_RCPCfA5aq9ojSwk5Y2EmClBPs",
};

const ecPublicJwk = (pub) => ({ kty: "EC", crv: "P-256", x: b64urlEncode(pub.slice(1, 33)), y: b64urlEncode(pub.slice(33, 65)), ext: true });

// Independent decryption (node:crypto) of an aes128gcm body with the UA key.
function decryptAes128gcm(body, uaPrivateB64, uaPublic, authB64) {
  const salt = body.subarray(0, 16);
  const idlen = body[20];
  const asPub = body.subarray(21, 21 + idlen);
  const ct = body.subarray(21 + idlen);
  const ecdh = createECDH("prime256v1");
  ecdh.setPrivateKey(b64urlDecode(uaPrivateB64));
  const shared = ecdh.computeSecret(asPub);
  const keyInfo = cat(Buffer.from("WebPush: info\0"), uaPublic, asPub);
  const ikm = Buffer.from(hkdfSync("sha256", shared, b64urlDecode(authB64), keyInfo, 32));
  const cek = Buffer.from(hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: aes128gcm\0"), 16));
  const nonce = Buffer.from(hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: nonce\0"), 12));
  const d = createDecipheriv("aes-128-gcm", cek, nonce);
  d.setAuthTag(ct.subarray(ct.length - 16));
  const padded = Buffer.concat([d.update(ct.subarray(0, ct.length - 16)), d.final()]);
  return padded.subarray(0, padded.length - 1); // strip the 0x02 delimiter
}

// --- RFC 8291 ---
const asPublic = b64urlDecode(RFC.asPublic);
const uaPublic = b64urlDecode(RFC.uaPublic);
const auth = b64urlDecode(RFC.auth);
const salt = b64urlDecode(RFC.salt);
const sectionBody = b64urlDecode(RFC.body);
check("RFC 8291 section 5 body decodes to 144 octets (header 86 + 58)", sectionBody.length === 144, "length " + sectionBody.length);
check("RFC 8291 section 5 header matches appendix A", b64urlEncode(sectionBody.subarray(0, 86)) === RFC.header);

const decrypted = decryptAes128gcm(sectionBody, RFC.uaPrivate, uaPublic, RFC.auth);
check("independent decrypt of the RFC 8291 section 5 body yields the plaintext", decrypted.toString() === RFC.plaintext, decrypted.toString());

const asPrivateKey = await crypto.subtle.importKey("jwk", { ...ecPublicJwk(asPublic), d: RFC.asPrivate, key_ops: ["deriveBits"] }, { name: "ECDH", namedCurve: "P-256" }, false, ["deriveBits"]);
const encrypted = await encryptPayload(new TextEncoder().encode(RFC.plaintext), uaPublic, auth, {
  publicKey: asPublic, privateKey: asPrivateKey, salt,
});
check("encryptPayload reproduces the RFC 8291 section 5 body byte for byte", bytesEq(encrypted, sectionBody),
  "got " + b64urlEncode(encrypted).slice(0, 40) + "...");
check("encryptPayload output header and ciphertext match appendix A", b64urlEncode(encrypted.subarray(86)) === RFC.ciphertext);

// Production path (fresh ephemeral key and salt): round trip through the UA key.
const uaPair = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
const uaPubRaw = new Uint8Array(await crypto.subtle.exportKey("raw", uaPair.publicKey));
const uaJwk = await crypto.subtle.exportKey("jwk", uaPair.privateKey);
const freshAuth = crypto.getRandomValues(new Uint8Array(16));
const fresh = await encryptPayload(new TextEncoder().encode('{"title":"t"}'), uaPubRaw, freshAuth);
const freshDec = decryptAes128gcm(fresh, uaJwk.d, uaPubRaw, b64urlEncode(freshAuth));
check("fresh-key encryption decrypts with the subscription keys", freshDec.toString() === '{"title":"t"}', freshDec.toString());

// --- RFC 8292 ---
const rfcPub = await crypto.subtle.importKey("jwk", ecPublicJwk(b64urlDecode(RFC.vapidK)), { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
const [rh, rp, rs] = RFC.vapidT.split(".");
const rfcVerifies = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, rfcPub, b64urlDecode(rs), new TextEncoder().encode(rh + "." + rp));
check("RFC 8292 example JWT verifies under its k", rfcVerifies === true);

const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
const pubRaw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
const privJwk = await crypto.subtle.exportKey("jwk", pair.privateKey);
const keys = await importVapid(b64urlEncode(pubRaw), privJwk.d, "mailto:owner@example.invalid");
const now = Date.UTC(2026, 10, 16, 12, 0, 0);
const token = await vapidToken(keys, "https://fcm.googleapis.com", now);
const [h, p, s] = token.split(".");
const claims = JSON.parse(Buffer.from(p, "base64url").toString());
check("VAPID header is ES256 JWT", Buffer.from(h, "base64url").toString() === '{"typ":"JWT","alg":"ES256"}');
check("VAPID claims carry aud, sub and a 12 h exp", claims.aud === "https://fcm.googleapis.com" && claims.sub === "mailto:owner@example.invalid" && claims.exp === Math.floor(now / 1000) + 12 * 3600, JSON.stringify(claims));
const ourVerifies = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, pair.publicKey, b64urlDecode(s), new TextEncoder().encode(h + "." + p));
check("our VAPID JWT verifies under the public key", ourVerifies === true);
check("VAPID signature is the 64-octet raw r||s form", b64urlDecode(s).length === 64);

// --- allow-list, keys, notify frames ---
const allowed = (e, t) => pushEndpointAllowed(e, t);
check("fcm endpoint allowed", allowed("https://fcm.googleapis.com/fcm/send/abc"));
check("mozilla endpoint allowed", allowed("https://updates.push.services.mozilla.com/wpush/v2/abc"));
check("apple subdomain allowed", allowed("https://web.push.apple.com/QAbc"));
check("windows subdomain allowed", allowed("https://wns2-pn1p.notify.windows.com/w/?token=x"));
check("bare apple suffix refused", !allowed("https://push.apple.com/x"));
check("arbitrary https host refused", !allowed("https://example.com/push"));
check("lookalike host refused", !allowed("https://fcm.googleapis.com.evil.example/x"));
check("http refused without the test origin", !allowed("http://fcm.googleapis.com/x"));
check("explicit port refused", !allowed("https://fcm.googleapis.com:8443/x"));
check("credentials refused", !allowed("https://u:p@fcm.googleapis.com/x"));
check("test origin allowed only when given", allowed("http://127.0.0.1:9999/push/1", "http://127.0.0.1:9999") && !allowed("http://127.0.0.1:9999/push/1"));
check("unparsable endpoint refused", !allowed("not a url"));

check("subscription keys: 65-byte point and 16-byte secret", validSubscriptionKeys(RFC.uaPublic, RFC.auth));
check("subscription keys: compressed point refused", !validSubscriptionKeys(b64urlEncode(uaPublic.subarray(0, 33)), RFC.auth));
check("subscription keys: short secret refused", !validSubscriptionKeys(RFC.uaPublic, b64urlEncode(auth.subarray(0, 8))));

const sid = "0f1e2d3c-4b5a-4968-8777-665544332211";
check("notify frame parsed", JSON.stringify(parseNotify(JSON.stringify({ type: "notify", session: sid.toUpperCase(), kind: "approval", text: "approval needed: bash" }))) === JSON.stringify({ session: sid, kind: "approval", text: "approval needed: bash" }));
check("non-notify frame ignored", parseNotify(JSON.stringify({ type: "tool", session: sid, kind: "approval", text: "x" })) === null);
check("unknown notify kind ignored", parseNotify(JSON.stringify({ type: "notify", session: sid, kind: "other", text: "x" })) === null);
check("notify without a session ignored", parseNotify(JSON.stringify({ type: "notify", kind: "done", text: "x" })) === null);
check("empty notify text ignored", parseNotify(JSON.stringify({ type: "notify", session: sid, kind: "done", text: "" })) === null);
check("garbage ignored", parseNotify("{nope") === null);

console.log(failures === 0 ? "ALL PASS" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
