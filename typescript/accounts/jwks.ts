// The accounts service's published signing keys, cached. An unknown `kid` triggers one refetch (keys rotate),
// at most once a minute.

import { AccountsError } from "./error.ts";

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

/** A key cache: remote (fetched from the issuer) or pinned (tests, offline use; never refetched). */
export class Keys {
  #url: string | null;
  #fetch: typeof fetch;
  #cache: { at: number; set: JwkSet } | null;
  #imported = new Map<string, Promise<CryptoKey>>();

  private constructor(url: string | null, set: JwkSet | null, fetcher: typeof fetch) {
    this.#url = url;
    this.#fetch = fetcher;
    this.#cache = set ? { at: Date.now(), set } : null;
  }

  /** Keys fetched from `url`. */
  static remote(url: string, fetcher: typeof fetch): Keys {
    return new Keys(url, null, fetcher);
  }

  /** Fixed keys, never fetched. */
  static pinned(set: JwkSet): Keys {
    return new Keys(null, set, fetch);
  }

  #lookup(kid: string): Promise<CryptoKey> | null {
    const jwk = this.#cache?.set.keys.find((k) => k.kid === kid && k.kty === "EC" && k.crv === "P-256");
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

  /** The key for `kid`, fetching the key set if it's missing or stale. */
  async get(kid: string): Promise<CryptoKey> {
    const age = this.#cache ? Date.now() - this.#cache.at : null;
    const stale = age === null || age > REFRESH_EVERY_MS;
    if (!stale || this.#url === null) {
      const key = this.#lookup(kid);
      if (key) return await key;
      if (this.#url === null) throw new AccountsError("invalid_token", "token rejected: unknown signing key");
    }
    // Unknown kid on a fresh cache: refetch, but not more than once a minute.
    if (stale || (age !== null && age > MIN_REFETCH_MS)) {
      let response: Response;
      try {
        response = await this.#fetch(this.#url!, { signal: AbortSignal.timeout(10_000) });
      } catch (cause) {
        throw new AccountsError("http", `http: ${cause}`, { cause });
      }
      if (!response.ok) {
        throw new AccountsError("http", `http: JWKS fetch returned ${response.status}`, { status: response.status });
      }
      this.#cache = { at: Date.now(), set: (await response.json()) as JwkSet };
    }
    const key = this.#lookup(kid);
    if (!key) throw new AccountsError("invalid_token", "token rejected: unknown signing key");
    return await key;
  }
}
