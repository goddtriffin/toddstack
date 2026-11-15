// Verifying webhooks from the accounts service.
//
// Each delivery is a POST with JSON and `X-Accounts-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of
// "<t>.<raw body>">` under your app's webhook secret. Verify against the raw body, before parsing. Deliveries
// can repeat (retries); use the event `id` to ignore duplicates.

import { utf8 } from "./encoding.ts";
import { AccountsError } from "./error.ts";

/** The header name. */
export const WEBHOOK_HEADER = "X-Accounts-Signature";

/** How old a signature may be, seconds. */
export const WEBHOOK_TOLERANCE_SECONDS = 300;

/** A verified event. */
export interface WebhookEvent {
  /** Unique per event; repeated on retries. */
  id: string;
  /** `user.updated`, `user.deleted`, `user.suspended`, `user.reactivated`, `user.signed_out`, or `grant.revoked`. */
  type: string;
  /** Unix seconds. */
  createdAt: number;
  /** Always has `sub` (the person the event is about); `user.updated` adds the fields that changed. */
  data: { sub?: string; [field: string]: unknown };
}

function reject(why: string): AccountsError {
  return new AccountsError("invalid_webhook", `webhook rejected: ${why}`);
}

function fromHex(hex: string): Uint8Array<ArrayBuffer> | null {
  if (hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) return null;
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/**
 * Checks the signature and age, then parses the event. `body` is the raw request body (text or bytes);
 * `now` is the current Unix time in seconds (a parameter so tests can fix it).
 */
export async function verifyWebhook(
  secret: string | Uint8Array,
  signatureHeader: string | null | undefined,
  body: string | Uint8Array,
  now: number = Math.floor(Date.now() / 1000),
): Promise<WebhookEvent> {
  let t: number | null = null;
  let v1: Uint8Array<ArrayBuffer> | null = null;
  for (const part of (signatureHeader ?? "").split(",")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    const [k, value] = [part.slice(0, eq).trim(), part.slice(eq + 1).trim()];
    if (k === "t") t = /^-?\d+$/.test(value) ? Number(value) : null;
    if (k === "v1") v1 = fromHex(value);
  }
  if (t === null || v1 === null) throw reject("malformed signature header");
  if (Math.abs(now - t) > WEBHOOK_TOLERANCE_SECONDS) throw reject("signature too old (or from the future)");
  const bodyBytes = typeof body === "string" ? utf8(body) : body;
  const signed = new Uint8Array([...utf8(`${t}.`), ...bodyBytes]);
  const keyBytes = typeof secret === "string" ? utf8(secret) : new Uint8Array(secret);
  const key = await crypto.subtle.importKey("raw", keyBytes, { name: "HMAC", hash: "SHA-256" }, false, ["verify"]);
  // WebCrypto's verify compares in constant time.
  if (!(await crypto.subtle.verify("HMAC", key, v1, signed))) throw reject("signature mismatch");
  let raw: { id?: unknown; type?: unknown; created_at?: unknown; data?: unknown };
  try {
    raw = JSON.parse(new TextDecoder().decode(bodyBytes));
  } catch {
    throw reject("body is not an event");
  }
  if (
    typeof raw?.id !== "string" || typeof raw.type !== "string" || typeof raw.created_at !== "number" ||
    raw.data === undefined
  ) {
    throw reject("body is not an event");
  }
  return { id: raw.id, type: raw.type, createdAt: raw.created_at, data: raw.data as WebhookEvent["data"] };
}
