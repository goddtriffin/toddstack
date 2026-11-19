// Instant logout for servers that verify access tokens locally.
//
// `accounts.verifyAccessToken` never calls the accounts service, so on its own it cannot see a sign-out. The
// service tells you which token families (`fam`) are dead, two ways, and both feed one in-memory set (`accounts.revocations`)
// that `verifyAccessToken` consults: the signed `tokens.revoked` webhook (`accounts.handleWebhook`), and a poll
// of `GET /api/v1/revoked-tokens` at startup and every 30 seconds, the backup for a missed webhook
// (`accounts.startRevocationPoller`). Entries are kept until their `expires_at` (when the family's last access token
// has died), so the set stays small. If accounts is unreachable, verification keeps working (fail open) and
// `onError` hears about each failed poll.

import type { WebhookEvent } from "./webhook.ts";

/** How often the poller asks for new revocations. */
export const REVOCATION_POLL_INTERVAL_MS = 30_000;

/**
 * How long a family from a webhook is kept: the access token lifetime (10 minutes) plus the service's own margin,
 * rounded up. The webhook carries no `expires_at`.
 */
export const WEBHOOK_REVOCATION_TTL_SECONDS = 15 * 60;

/** Unix seconds of an RFC 3339 time, or `null` if it doesn't parse. */
export function unixSeconds(rfc3339: string): number | null {
  const ms = Date.parse(rfc3339);
  return Number.isNaN(ms) ? null : Math.floor(ms / 1000);
}

const nowSeconds = () => Math.floor(Date.now() / 1000);

/** The token families known to be revoked, shared by the webhook handler, the poller and token verification. */
export class Revocations {
  /** `fam` to the Unix second it stops mattering. */
  #families = new Map<string, number>();

  /** Records a dead family until `expiresAt` (Unix seconds). Repeats are fine (polls overlap): the later expiry wins. */
  revoke(fam: string, expiresAt: number): void {
    const now = nowSeconds();
    for (const [key, until] of this.#families) if (until <= now) this.#families.delete(key);
    this.#families.set(fam, Math.max(this.#families.get(fam) ?? expiresAt, expiresAt));
  }

  /** Whether `fam` is revoked and its access tokens could still be alive. */
  isRevoked(fam: string): boolean {
    const until = this.#families.get(fam);
    return until !== undefined && until > nowSeconds();
  }

  /** How many families are being held. */
  get size(): number {
    const now = nowSeconds();
    let n = 0;
    for (const until of this.#families.values()) if (until > now) n++;
    return n;
  }

  /** Records the families of a verified `tokens.revoked` webhook; any other event is ignored. Returns how many it named. */
  apply(event: WebhookEvent): number {
    if (event.type !== "tokens.revoked") return 0;
    const families = Array.isArray(event.data.families)
      ? event.data.families.filter((f): f is string => typeof f === "string")
      : [];
    const until = nowSeconds() + WEBHOOK_REVOCATION_TTL_SECONDS;
    for (const fam of families) this.revoke(fam, until);
    return families.length;
  }
}

/** How the poller runs. */
export interface RevocationOptions {
  /** Milliseconds between polls. Defaults to 30 seconds. */
  intervalMs?: number;
  /** Called with each failed poll (the poller retries on its own). Log it. */
  onError?: (error: unknown) => void;
}

/** What the poller needs from the client. */
interface Pollable {
  pollRevocations(since: string | null): Promise<{ next: string | null; listed: number }>;
}

/** The running poll. Call `stop()` at shutdown. */
export class RevocationPoller {
  #timer: ReturnType<typeof setTimeout> | undefined;
  #stopped = false;

  private constructor() {}

  /** One poll now (awaited), then one per interval. A failed poll goes to `onError`. */
  static async start(accounts: Pollable, options: RevocationOptions = {}): Promise<RevocationPoller> {
    const poller = new RevocationPoller();
    const interval = options.intervalMs ?? REVOCATION_POLL_INTERVAL_MS;
    let cursor: string | null = null;
    const poll = async () => {
      try {
        cursor = (await accounts.pollRevocations(cursor)).next;
      } catch (error) {
        options.onError?.(error);
      }
    };
    const tick = async () => {
      await poll();
      if (!poller.#stopped) poller.#timer = setTimeout(tick, interval);
    };
    await poll();
    poller.#timer = setTimeout(tick, interval);
    return poller;
  }

  /** Stops polling. */
  stop(): void {
    this.#stopped = true;
    clearTimeout(this.#timer);
  }
}
