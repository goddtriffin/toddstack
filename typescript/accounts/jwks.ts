// The accounts service's published signing keys, cached. Keys rotate on a schedule (a new key is published
// before it signs), so an unknown `kid` triggers a refetch, at most once a minute whether or not it succeeds: a
// burst of tokens with made-up `kid`s can't turn into a burst of fetches. Concurrent callers share one fetch and
// its outcome.

import { AccountsError } from "./error.ts";
import { READ, type RetryPolicy, send } from "./retry.ts";

/** One key from a JWKS document. */
export interface Jwk extends JsonWebKey {
  /** Key id (the JWT header's `kid`). */
  kid?: string;
}

/** A JWKS document. */
export interface JwkSet {
  /** The keys. */
  keys: Jwk[];
}

const REFRESH_EVERY_MS = 3600_000;
const MIN_REFETCH_MS = 60_000;

const unknown = () => new AccountsError("invalid_token", "token rejected: unknown signing key");

/** A key cache: remote (fetched from the issuer) or pinned (tests, offline use; never refetched). */
export class Keys {
  #url: string | null;
  #fetch: typeof fetch;
  #now: () => number;
  #set: JwkSet | null;
  #fetchedAt: number | null;
  /** The last fetch made for an unknown `kid`, successful or not. */
  #refetchedAt: number | null = null;
  #inFlight: Promise<void> | null = null;
  #imported = new Map<string, Promise<CryptoKey>>();

  #policy: RetryPolicy;

  private constructor(
    url: string | null,
    set: JwkSet | null,
    fetcher: typeof fetch,
    now: () => number,
    policy: RetryPolicy,
  ) {
    this.#url = url;
    this.#fetch = fetcher;
    this.#now = now;
    this.#set = set;
    this.#fetchedAt = set ? now() : null;
    this.#policy = policy;
  }

  /**
   * Keys fetched from `url`. `now` (milliseconds) and `policy` are parameters so tests can move the clock and keep
   * retries fast.
   */
  static remote(url: string, fetcher: typeof fetch, now: () => number = Date.now, policy: RetryPolicy = READ): Keys {
    return new Keys(url, null, fetcher, now, policy);
  }

  /** Fixed keys, never fetched. */
  static pinned(set: JwkSet): Keys {
    return new Keys(null, set, fetch, Date.now, READ);
  }

  #lookup(kid: string): Promise<CryptoKey> | null {
    const jwk = this.#set?.keys.find((k) => k.kid === kid && k.kty === "EC" && k.crv === "P-256");
    if (!jwk) return null;
    const id = JSON.stringify([jwk.kid, jwk.x, jwk.y]);
    let key = this.#imported.get(id);
    if (!key) {
      const { kid: _kid, alg: _alg, use: _use, key_ops: _ops, ...material } = jwk;
      key = crypto.subtle.importKey("jwk", material, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
      this.#imported.set(id, key);
    }
    return key;
  }

  #fresh(): boolean {
    return this.#fetchedAt !== null && this.#now() - this.#fetchedAt <= REFRESH_EVERY_MS;
  }

  async #fetchSet(url: string): Promise<void> {
    let response: Response;
    try {
      // One logical fetch, retried on an intermittent failure; the once-a-minute limit counts it once.
      response = await send(this.#fetch, url, {}, this.#policy);
    } catch (cause) {
      throw new AccountsError("http", `http: ${cause}`, { cause });
    }
    if (!response.ok) {
      throw new AccountsError("http", `http: JWKS fetch returned ${response.status}`, { status: response.status });
    }
    const set = (await response.json()) as JwkSet;
    this.#set = set;
    this.#fetchedAt = this.#refetchedAt = this.#now();
  }

  /** The key for `kid`, fetching the key set if it's stale, or if `kid` is new to it (rate-limited). */
  async get(kid: string): Promise<CryptoKey> {
    if (this.#url === null) return await (this.#lookup(kid) ?? Promise.reject(unknown()));
    if (this.#fresh()) {
      const key = this.#lookup(kid);
      if (key) return await key;
    }
    if (this.#inFlight) {
      // Share the fetch already running, and its outcome.
      await this.#inFlight.catch(() => {});
      if (!this.#fresh()) {
        throw new AccountsError("invalid_token", "token rejected: the signing keys couldn't be fetched (try again)");
      }
      return await (this.#lookup(kid) ?? Promise.reject(unknown()));
    }
    if (this.#fresh()) {
      if (this.#refetchedAt !== null && this.#now() - this.#refetchedAt <= MIN_REFETCH_MS) throw unknown();
      this.#refetchedAt = this.#now();
    }
    this.#inFlight = this.#fetchSet(this.#url);
    try {
      await this.#inFlight;
    } finally {
      this.#inFlight = null;
    }
    return await (this.#lookup(kid) ?? Promise.reject(unknown()));
  }
}
