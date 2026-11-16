// Retries for reads from the accounts service.
//
// Only a GET is ever retried, and only on an intermittent failure: a network error, a timeout, a 5xx or a 429.
// Anything else is an answer, and asking again would get the same one.
//
// Everything else is sent exactly once, and must stay that way: the token exchange (an authorization code is
// single-use), refresh (rotation: a retried refresh that had in fact landed presents a spent token and signs the
// person out), revocation, and every POST, PUT and DELETE (a retried event or friend request that had landed the
// first time is a duplicate). `send` enforces this itself, so a caller cannot opt a write in by mistake.

/** How hard to try before giving up. */
export interface RetryPolicy {
  /** Attempts in all, the first included. */
  attempts: number;
  /** Each attempt's own deadline, so one hung connection cannot use up the whole budget. */
  attemptTimeoutMs: number;
  /** The backoff ceiling for the first retry; it doubles per retry. */
  baseDelayMs: number;
  maxDelayMs: number;
  /** A `Retry-After` longer than this is obeyed by not retrying at all. */
  maxRetryAfterMs: number;
  /** No retry starts once this much time has passed. */
  budgetMs: number;
}

export const READ: RetryPolicy = {
  attempts: 3,
  attemptTimeoutMs: 3_000,
  baseDelayMs: 200,
  maxDelayMs: 1_000,
  maxRetryAfterMs: 2_000,
  budgetMs: 6_000,
};

/** The deadline for a request that is sent once. */
const WRITE_TIMEOUT_MS = 10_000;

/** Full jitter: uniformly random in `[0, min(maxDelay, baseDelay * 2^(retry - 1))]`. */
export function backoff(policy: RetryPolicy, retry: number): number {
  const ceiling = Math.min(policy.maxDelayMs, policy.baseDelayMs * 2 ** Math.min(retry - 1, 16));
  return Math.floor(Math.random() * (ceiling + 1));
}

export function isRetryableStatus(status: number): boolean {
  return status === 429 || (status >= 500 && status < 600);
}

/** `Retry-After` in delta-seconds, as milliseconds. The HTTP-date form falls back to backoff. */
function retryAfterMs(response: Response): number | null {
  const value = response.headers.get("retry-after")?.trim();
  if (!value || !/^\d+$/.test(value)) return null;
  return Number(value) * 1_000;
}

/** A network failure or a timeout: worth another attempt. */
function isIntermittent(error: unknown): boolean {
  if (error instanceof TypeError) return true;
  return error instanceof DOMException && (error.name === "TimeoutError" || error.name === "AbortError");
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/**
 * Sends a request: once for anything but a GET, and under `policy` for a GET. Resolves with the last response (a
 * success, a final answer, or the last retryable one once attempts or the budget run out) and rejects with the last
 * error.
 */
export async function send(
  fetcher: typeof fetch,
  url: string,
  init: RequestInit = {},
  policy: RetryPolicy = READ,
): Promise<Response> {
  if ((init.method ?? "GET").toUpperCase() !== "GET") {
    return await fetcher(url, { ...init, signal: AbortSignal.timeout(WRITE_TIMEOUT_MS) });
  }
  const started = Date.now();
  for (let retry = 1;; retry++) {
    let wait: number | null = null;
    try {
      const response = await fetcher(url, { ...init, signal: AbortSignal.timeout(policy.attemptTimeoutMs) });
      if (retry >= policy.attempts || !isRetryableStatus(response.status)) return response;
      const retryAfter = retryAfterMs(response);
      if (retryAfter !== null && retryAfter > policy.maxRetryAfterMs) return response;
      wait = retryAfter ?? backoff(policy, retry);
      if (Date.now() - started + wait > policy.budgetMs) return response;
      // Discarded: release the connection rather than leave the body unread.
      await response.body?.cancel();
    } catch (error) {
      if (retry >= policy.attempts || !isIntermittent(error)) throw error;
      wait = backoff(policy, retry);
      if (Date.now() - started + wait > policy.budgetMs) throw error;
    }
    await sleep(wait);
  }
}
