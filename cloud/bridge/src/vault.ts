// Provider-key vault helpers (PLAN 10.9). Pure WebCrypto and plain data: no
// Cloudflare import, so test/vault-unit.mjs can load this file directly. The
// Directory object stores only what sealCredential returns; the Worker holds
// VAULT_KEY and does all sealing and opening. Nothing here logs or returns a
// plaintext value except openCredential, whose caller is the host route.
//
// Scheme: AES-256-GCM. The key is HKDF-SHA256 of VAULT_KEY (salt
// "modron-vault-v1", info "modron-vault-aes-gcm-v1"). Each write draws a fresh
// random 96-bit nonce. The provider id is the additional authenticated data,
// so a ciphertext moved to another provider's row fails to open.

// Provider ids the vault accepts, each mapped to the environment variable it
// stands for on a host. The ids are the auth catalog's (provider_registry in
// crates/modron-core/src/auth.rs); keep the two in step.
export const VAULT_PROVIDERS: Readonly<Record<string, string>> = {
  anthropic: "ANTHROPIC_API_KEY",
  "openai-compat": "OPENAI_API_KEY",
  zai: "ZAI_API_KEY",
  google: "GEMINI_API_KEY",
  groq: "GROQ_API_KEY",
  openrouter: "OPENROUTER_API_KEY",
  mistral: "MISTRAL_API_KEY",
  cerebras: "CEREBRAS_API_KEY",
  nvidia: "NVIDIA_API_KEY",
  "github-models": "GITHUB_MODELS_TOKEN",
};

// Longest key accepted (UTF-8 bytes). Real provider keys are far shorter.
export const VAULT_VALUE_MAX_BYTES = 512;

const CONTROL_RE = /[\u0000-\u001f\u007f]/;
const enc = new TextEncoder();
const dec = new TextDecoder();
const SALT = enc.encode("modron-vault-v1");
const INFO = enc.encode("modron-vault-aes-gcm-v1");

export type Checked<T> = { ok: true; value: T } | { ok: false; message: string };

// The environment variable for a vault provider id, or null when the id is
// not in the allow-list.
export function providerEnvVar(id: string): string | null {
  return Object.prototype.hasOwnProperty.call(VAULT_PROVIDERS, id) ? VAULT_PROVIDERS[id] : null;
}

// A key as the owner typed it: a non-empty string, at most 512 bytes, with no
// control characters. The message never repeats the value.
export function checkCredentialValue(value: unknown): Checked<string> {
  if (typeof value !== "string") return { ok: false, message: "key must be a string" };
  if (value.length === 0) return { ok: false, message: "key must not be empty" };
  if (enc.encode(value).length > VAULT_VALUE_MAX_BYTES) {
    return { ok: false, message: `key must be at most ${VAULT_VALUE_MAX_BYTES} bytes` };
  }
  if (CONTROL_RE.test(value)) return { ok: false, message: "key has control characters" };
  return { ok: true, value };
}

export interface Sealed {
  nonce: Uint8Array;
  ciphertext: Uint8Array;
}

async function vaultAesKey(vaultSecret: string): Promise<CryptoKey> {
  const ikm = await crypto.subtle.importKey("raw", enc.encode(vaultSecret), "HKDF", false, ["deriveKey"]);
  return crypto.subtle.deriveKey(
    { name: "HKDF", hash: "SHA-256", salt: SALT, info: INFO },
    ikm,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

// Seals one value under an associated-data label (a provider id for
// provider keys, an account id for GitHub tokens) with a fresh nonce.
export async function sealValue(vaultSecret: string, aad: string, value: string): Promise<Sealed> {
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: nonce, additionalData: enc.encode(aad), tagLength: 128 },
    await vaultAesKey(vaultSecret),
    enc.encode(value),
  );
  return { nonce, ciphertext: new Uint8Array(ct) };
}

// Opens a sealed value under the same label. Throws on a wrong VAULT_KEY, a
// wrong label, or any change to the nonce or ciphertext. Callers catch and
// report "unreadable".
export async function openValue(vaultSecret: string, aad: string, sealed: Sealed): Promise<string> {
  const pt = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: sealed.nonce, additionalData: enc.encode(aad), tagLength: 128 },
    await vaultAesKey(vaultSecret),
    sealed.ciphertext,
  );
  return dec.decode(pt);
}

// Seals one provider key for one provider id.
export function sealCredential(vaultSecret: string, providerId: string, value: string): Promise<Sealed> {
  return sealValue(vaultSecret, providerId, value);
}

// Opens one provider key sealed for `providerId`.
export function openCredential(vaultSecret: string, providerId: string, sealed: Sealed): Promise<string> {
  return openValue(vaultSecret, providerId, sealed);
}
