// Verifying webhooks from the accounts service.
//
// Each delivery is a POST with JSON and `X-Accounts-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256 of
// "<t>.<raw body>">` under your app's webhook secret. While a secret is being rotated the header carries several
// `v1=` values, one per secret; the event is accepted when any one matches. Verify against the raw body, before parsing. Deliveries
// can repeat (retries); use the event `id` to ignore duplicates.

import { utf8 } from "./encoding.ts";
import { AccountsError } from "./error.ts";

/** The header name. */
export const WEBHOOK_HEADER = "X-Accounts-Signature";

/** How old a signature may be, seconds. */
export const WEBHOOK_TOLERANCE_SECONDS = 300;

/**
 * Every event type this version knows, as sent on the wire:
 *
 * - `user.updated`: `data` adds the fields that changed.
 * - `user.deleted`: delete or anonymize the person's rows.
 * - `user.suspended`, `user.signed_out`, `grant.revoked`: end the person's sessions.
 * - `user.reactivated`.
 * - `friendship.created`, `friendship.removed`: `data.sub` and `data.friend_sub` became (or stopped being)
 *   friends. Sent once per side, so each person gets an event with themselves as `sub`.
 * - `friend_request.created`, `friend_request.cancelled`, `friend_request.declined`: `data.from_sub` asked
 *   `data.to_sub`, took the request back, or was turned down. Sent once per side. An accepted request sends
 *   only `friendship.created`.
 */
export const WEBHOOK_EVENT_TYPES = [
  "user.updated",
  "user.deleted",
  "user.suspended",
  "user.reactivated",
  "user.signed_out",
  "grant.revoked",
  "friendship.created",
  "friendship.removed",
  "friend_request.created",
  "friend_request.cancelled",
  "friend_request.declined",
] as const;

/** An event type this version knows (see `WEBHOOK_EVENT_TYPES`). */
export type WebhookEventType = typeof WEBHOOK_EVENT_TYPES[number];

/** Whether `type` is one this version knows. Newer types still verify; they just aren't in the union. */
export function isKnownWebhookEventType(type: string): type is WebhookEventType {
  return (WEBHOOK_EVENT_TYPES as readonly string[]).includes(type);
}

/** A verified event. */
export interface WebhookEvent {
  /** Unique per event; repeated on retries. */
  id: string;
  /** One of `WEBHOOK_EVENT_TYPES`, or a type newer than this version (still verified, still delivered). */
  // The intersection keeps editor completion for the known types while accepting newer ones.
  type: WebhookEventType | (string & Record<never, never>);
  /** Unix seconds. */
  createdAt: number;
  /**
   * Always has `sub` (the person the event is about); `user.updated` adds the fields that changed, and the
   * friendship events add `friend_sub` (the other person), and the friend request events add `from_sub` (who
   * asked) and `to_sub` (who was asked).
   */
  data: { sub?: string; friend_sub?: string; from_sub?: string; to_sub?: string; [field: string]: unknown };
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
 * Checks the signature and age, then parses the event. An empty secret is refused. The header may carry several
 * `v1=` signatures (secret rotation): the event is accepted when any one matches `secret`. `body` is the raw request body (text or bytes);
 * `now` is the current Unix time in seconds (a parameter so tests can fix it).
 */
export async function verifyWebhook(
  secret: string | Uint8Array,
  signatureHeader: string | null | undefined,
  body: string | Uint8Array,
  now: number = Math.floor(Date.now() / 1000),
): Promise<WebhookEvent> {
  if (secret.length === 0) throw reject("empty secret");
  let t: number | null = null;
  const candidates: Uint8Array<ArrayBuffer>[] = [];
  for (const part of (signatureHeader ?? "").split(",")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    const [k, value] = [part.slice(0, eq).trim(), part.slice(eq + 1).trim()];
    if (k === "t") t = /^-?\d+$/.test(value) ? Number(value) : null;
    if (k === "v1") {
      const decoded = fromHex(value);
      if (decoded !== null) candidates.push(decoded);
    }
  }
  if (t === null || candidates.length === 0) throw reject("malformed signature header");
  if (Math.abs(now - t) > WEBHOOK_TOLERANCE_SECONDS) throw reject("signature too old (or from the future)");
  const bodyBytes = typeof body === "string" ? utf8(body) : body;
  const signed = new Uint8Array([...utf8(`${t}.`), ...bodyBytes]);
  const keyBytes = typeof secret === "string" ? utf8(secret) : new Uint8Array(secret);
  const key = await crypto.subtle.importKey("raw", keyBytes, { name: "HMAC", hash: "SHA-256" }, false, ["verify"]);
  // WebCrypto's verify compares in constant time. Every candidate is checked, none skipped on a match, so timing
  // doesn't say which one matched.
  let matched = false;
  for (const candidate of candidates) {
    if (await crypto.subtle.verify("HMAC", key, candidate, signed)) matched = true;
  }
  if (!matched) throw reject("signature mismatch");
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
