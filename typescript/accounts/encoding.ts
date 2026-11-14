// Base64url, UTF-8 and random helpers on Web Platform APIs only (Node 20+, Deno, Bun, browsers).

const encoder: TextEncoder = new TextEncoder();
const decoder: TextDecoder = new TextDecoder();

/** UTF-8 bytes of `text`. */
export function utf8(text: string): Uint8Array<ArrayBuffer> {
  return encoder.encode(text) as Uint8Array<ArrayBuffer>;
}

/** Text of UTF-8 `bytes`. */
export function fromUtf8(bytes: Uint8Array): string {
  return decoder.decode(bytes);
}

/** Unpadded base64url of `bytes`. */
export function base64url(bytes: Uint8Array): string {
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

/** Bytes of base64url (padded or not) or standard base64 `text`. Throws on malformed input. */
export function fromBase64(text: string): Uint8Array<ArrayBuffer> {
  const standard = text.replaceAll("-", "+").replaceAll("_", "/");
  const padded = standard + "=".repeat((4 - (standard.length % 4)) % 4);
  const binary = atob(padded);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

/** `n` cryptographically random bytes. */
export function randomBytes(n: number): Uint8Array<ArrayBuffer> {
  return crypto.getRandomValues(new Uint8Array(n));
}

/** A random base64url token with `n` bytes of entropy. */
export function randomToken(n = 32): string {
  return base64url(randomBytes(n));
}
