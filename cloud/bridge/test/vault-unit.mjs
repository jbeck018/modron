// Unit checks for src/vault.ts: the seal/open scheme and the value rules. No
// Worker runs here:  node --experimental-strip-types test/vault-unit.mjs
// (from cloud/bridge). The plaintext is searched for in the raw ciphertext
// bytes, which is what the Directory stores.
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const vault = await import(join(here, "..", "src", "vault.ts"));
const { checkCredentialValue, openCredential, providerEnvVar, sealCredential } = vault;

let failures = 0;
const check = (name, ok, detail = "") => {
  console.log((ok ? "PASS " : "FAIL ") + name + (ok || !detail ? "" : " -- " + detail));
  if (!ok) failures++;
};
const SECRET = "vault-secret-for-unit-tests-0123456789";
const VALUE = "sk-unit-plaintext-marker-7f3a9c";

const sealed = await sealCredential(SECRET, "anthropic", VALUE);
check("nonce is 96 bits", sealed.nonce.length === 12, String(sealed.nonce.length));
check("round trip opens to the value", (await openCredential(SECRET, "anthropic", sealed)) === VALUE);
check("ciphertext does not contain the plaintext", !Buffer.from(sealed.ciphertext).includes(VALUE));
check("ciphertext does not contain the value as utf-8 prefix", !Buffer.from(sealed.ciphertext).toString("latin1").includes("sk-unit"));
const again = await sealCredential(SECRET, "anthropic", VALUE);
check("each write draws a fresh nonce", Buffer.from(again.nonce).toString("hex") !== Buffer.from(sealed.nonce).toString("hex"));
check("each write produces different ciphertext", Buffer.from(again.ciphertext).toString("hex") !== Buffer.from(sealed.ciphertext).toString("hex"));

const wrongAad = await openCredential(SECRET, "openai-compat", sealed).then(() => "opened", () => "refused");
check("wrong provider id (AAD) fails to open", wrongAad === "refused", wrongAad);

const tampered = { nonce: sealed.nonce, ciphertext: Uint8Array.from(sealed.ciphertext) };
tampered.ciphertext[0] ^= 0x01;
const tamperedOpen = await openCredential(SECRET, "anthropic", tampered).then(() => "opened", () => "refused");
check("tampered ciphertext fails to open", tamperedOpen === "refused", tamperedOpen);

const tagFlip = { nonce: sealed.nonce, ciphertext: Uint8Array.from(sealed.ciphertext) };
tagFlip.ciphertext[tagFlip.ciphertext.length - 1] ^= 0x80;
check("tampered tag fails to open", (await openCredential(SECRET, "anthropic", tagFlip).then(() => "opened", () => "refused")) === "refused");

const nonceFlip = { nonce: Uint8Array.from(sealed.nonce), ciphertext: sealed.ciphertext };
nonceFlip.nonce[3] ^= 0x10;
check("tampered nonce fails to open", (await openCredential(SECRET, "anthropic", nonceFlip).then(() => "opened", () => "refused")) === "refused");

const otherKey = await openCredential(SECRET + "x", "anthropic", sealed).then(() => "opened", () => "refused");
check("a different VAULT_KEY fails to open", otherKey === "refused", otherKey);

check("allow-list maps anthropic to ANTHROPIC_API_KEY", providerEnvVar("anthropic") === "ANTHROPIC_API_KEY");
check("allow-list maps openai-compat to OPENAI_API_KEY", providerEnvVar("openai-compat") === "OPENAI_API_KEY");
check("allow-list refuses an unknown id", providerEnvVar("bogus") === null);
check("allow-list refuses a prototype key", providerEnvVar("__proto__") === null && providerEnvVar("constructor") === null);

check("accepts an ordinary key", checkCredentialValue("sk-abc_123").ok === true);
check("refuses an empty key", checkCredentialValue("").ok === false);
check("refuses a non-string", checkCredentialValue(42).ok === false);
check("refuses a key over 512 bytes", checkCredentialValue("a".repeat(513)).ok === false);
check("accepts exactly 512 bytes", checkCredentialValue("a".repeat(512)).ok === true);
check("counts multi-byte characters as bytes", checkCredentialValue("é".repeat(257)).ok === false);
check("refuses a newline", checkCredentialValue("sk-a\nb").ok === false);
check("refuses a NUL", checkCredentialValue("sk-a\u0000b").ok === false);
check("refuses DEL", checkCredentialValue("sk-a\u007fb").ok === false);
const refusal = checkCredentialValue("sk-a\nb");
check("the refusal message does not echo the value", refusal.ok === false && !refusal.message.includes("sk-a"));

console.log(failures === 0 ? "ALL PASS" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
