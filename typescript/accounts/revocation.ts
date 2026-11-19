// Instant logout for servers that verify access tokens locally.
//
// `accounts.verifyAccessToken` never calls the accounts service, so on its own it cannot see a sign-out. The
// service tells you which token families (`fam`) are dead, two ways, and both feed one in-memory set (`accounts.revocations`)
// that `verifyAccessToken` consults: the signed `tokens.revoked` webhook (`accounts.handleWebhook`), and a poll
// of `GET /api/v1/revoked-tokens` at startup and every 30 seconds, the backup for a missed webhook
// (`accounts.startRevocationPoller`). Entries are kept until their `expires_at` (when the family's last access token
// has died), so the set stays small. The set is empty until the first poll lands, so `startRevocationPoller` awaits it
// (retrying quickly) and rejects if it still fails: you decide whether to run without instant logout. After that, if
// accounts is unreachable, verification keeps working (fail open), `onError` hears about each failed poll, and
// `poller.lastSuccess()` tells a health check how stale the set is.

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

/** Expired entries are dropped at most this often, so a burst of inserts never rescans the whole set each time. */
const PRUNE_EVERY_SECONDS = 10;

/** The token families known to be revoked, shared by the webhook handler, the poller and token verification. */
export class Revocations {
  /** `fam` to the Unix second it stops mattering. */
  #families = new Map<string, number>();
  #nextPrune = 0;

  /**
   * Records a dead family until `expiresAt` (Unix seconds). Repeats are fine (polls overlap): the later expiry wins.
   * For many families use `revokeMany`.
   */
  revoke(fam: string, expiresAt: number): void {
    this.revokeMany([[fam, expiresAt]]);
  }

  /** `revoke` for a batch, pruning expired entries at most once per batch (and at most every few seconds). */
  revokeMany(families: Iterable<readonly [string, number]>): void {
    for (const [fam, expiresAt] of families) {
      this.#families.set(fam, Math.max(this.#families.get(fam) ?? expiresAt, expiresAt));
    }
    const now = nowSeconds();
    if (now >= this.#nextPrune) {
      for (const [key, until] of this.#families) if (until <= now) this.#families.delete(key);
      this.#nextPrune = now + PRUNE_EVERY_SECONDS;
    }
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
    this.revokeMany(families.map((fam) => [fam, until] as const));
    return families.length;
  }
}

/** How the poller runs. */
export interface RevocationOptions {
  /** Milliseconds between polls. Defaults to 30 seconds. */
  intervalMs?: number;
  /** Called with each failed poll (the poller retries on its own). Log it. If it throws, the poller carries on. */
  onError?: (error: unknown) => void;
  /** Milliseconds before the second startup try; the third waits twice as long. Defaults to 1 second. */
  startupBackoffMs?: number;
}

/** Tries at the startup poll before giving up. */
const STARTUP_TRIES = 3;

/** What the poller needs from the client. */
interface Pollable {
  pollRevocations(since: string | null): Promise<{ next: string | null; listed: number }>;
}

/** The running poll. Call `stop()` at shutdown. */
export class RevocationPoller {
  #timer: ReturnType<typeof setTimeout> | undefined;
  #stopped = false;
  #lastSuccess: Date | null = null;

  private constructor() {}

  /**
   * One poll now (awaited, up to 3 tries with backoff; rejects with the last error if all fail), then one per
   * interval. A later failed poll goes to `onError`.
   */
  static async start(accounts: Pollable, options: RevocationOptions = {}): Promise<RevocationPoller> {
    const poller = new RevocationPoller();
    const interval = options.intervalMs ?? REVOCATION_POLL_INTERVAL_MS;
    let cursor: string | null = null;
    const poll = async () => {
      cursor = (await accounts.pollRevocations(cursor)).next;
      poller.#lastSuccess = new Date();
    };
    let backoff = options.startupBackoffMs ?? 1_000;
    for (let attempt = 1;; attempt++) {
      try {
        await poll();
        break;
      } catch (error) {
        if (attempt >= STARTUP_TRIES) throw error;
        await new Promise((resolve) => setTimeout(resolve, backoff));
        backoff *= 2;
      }
    }
    const tick = async () => {
      try {
        await poll();
      } catch (error) {
        try {
          options.onError?.(error);
        } catch {
          // User code must not be able to end the poll.
        }
      }
      if (!poller.#stopped) poller.#timer = setTimeout(tick, interval);
    };
    poller.#timer = setTimeout(tick, interval);
    return poller;
  }

  /** When the last poll succeeded (the startup poll counts), or `null`. For a health check: a stale value means only webhooks are protecting you. */
  lastSuccess(): Date | null {
    return this.#lastSuccess;
  }

  /** Stops polling. */
  stop(): void {
    this.#stopped = true;
    clearTimeout(this.#timer);
  }
}
