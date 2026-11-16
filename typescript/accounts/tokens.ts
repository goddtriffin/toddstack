// Tokens: exchanging codes, refreshing, revoking, and verifying ID and access tokens.

import { fromBase64, fromUtf8, utf8 } from "./encoding.ts";
import { AccountsError } from "./error.ts";
import { Friends } from "./friends.ts";
import { type JwkSet, Keys } from "./jwks.ts";
import { endSessionUrl, type LogoutOptions, type SignOut } from "./logout.ts";
import { type LoginOptions, parseCallback, type PendingLogin, type StartedLogin, startLogin } from "./login.ts";
import { type Notification, notificationBody, readReceipt, type Receipt } from "./notifications.ts";
import { type GuestReport, reportBody } from "./report.ts";
import { KEYED_SEND, READ, type RetryPolicy, send } from "./retry.ts";
import { VERSION } from "./version.ts";

const TYPED_STATUS: Record<number, "not_found" | "unprocessable" | "rate_limited"> = {
  404: "not_found",
  422: "unprocessable",
  429: "rate_limited",
};

/** The production issuer. */
export const ISSUER = "https://accounts.toddgriffin.me/oidc";

/** Your app's registration with the accounts service. */
export interface Config {
  /** `https://accounts.toddgriffin.me/oidc` in production. */
  issuer: string;
  /** Your client id, as registered. */
  clientId: string;
  /** Null for public clients (native and desktop apps). */
  clientSecret: string | null;
  /** Exactly as registered. */
  redirectUri: string;
}

/** A config for the production accounts service. */
export function toddgriffin(clientId: string, clientSecret: string | null, redirectUri: string): Config {
  return { issuer: ISSUER, clientId, clientSecret, redirectUri };
}

/** The token endpoint's answer. */
export interface TokenSet {
  /** For calls to the accounts API (10 minutes). */
  accessToken: string;
  /** The signed ID token. */
  idToken: string;
  /**
   * Present when `offline_access` was granted. Rotates on every refresh: always store the new one, and never
   * reuse an old one (that revokes the whole family).
   */
  refreshToken: string | null;
  /** Access token lifetime, seconds. */
  expiresIn: number;
  /** The scopes granted, space-separated. */
  scope: string;
}

/** Verified ID token claims (standard claim names). */
export interface IdClaims {
  /** The person's permanent id. Key everything on this. */
  sub: string;
  /** Their username (scope `profile`). */
  username?: string;
  /** Display name (scope `profile`). */
  name?: string;
  /** Picture URL (scope `profile`). */
  picture?: string;
  /** Email (scope `email`). */
  email?: string;
  /** Whether the email was verified. */
  email_verified?: boolean;
  /** When they last proved who they are (Unix seconds). */
  auth_time?: number;
  /** How: `pwd`, `otp`, `hwk` (passkey), `fed` (social), `mfa`, ... */
  amr: string[];
  /** The nonce from the sign-in request. */
  nonce?: string;
  /** Any other claim. */
  [claim: string]: unknown;
}

/** Verified access token claims (RFC 9068). */
export interface AccessClaims {
  /** The person's permanent id. */
  sub: string;
  /** The app the token was issued to (always your client id once verified). */
  client_id: string;
  /** The scopes granted, space-separated; see `hasScope`. */
  scope: string;
  /** Expiry, Unix seconds. */
  exp: number;
  /** Any other claim. */
  [claim: string]: unknown;
}

/** Whether `claims` grant `scope`. */
export function hasScope(claims: AccessClaims, scope: string): boolean {
  return claims.scope.split(" ").includes(scope);
}

/** A finished sign-in. */
export interface SignedIn {
  /** Verified ID token claims. */
  claims: IdClaims;
  /** The tokens; store the refresh token if you need it. */
  tokens: TokenSet;
  /** The checked path from `startLogin`, if any. */
  returnTo: string | null;
}

/** Construction options. */
export interface AccountsOptions {
  /** Verify tokens against these keys and never fetch them: for tests, or a service that pins keys. */
  keys?: JwkSet;
  /** The `fetch` to use (defaults to the global one). */
  fetch?: typeof fetch;
}

interface RawTokenSet {
  access_token: string;
  id_token: string;
  refresh_token?: string | null;
  expires_in: number;
  scope: string;
}

/** Allowed clock skew for `exp`, seconds. */
const LEEWAY_SECONDS = 30;

/** The client. Create one per process and share it; it caches the signing keys. */
export class Accounts {
  /** This client's registration. */
  readonly config: Config;
  #fetch: typeof fetch;
  #keys: Keys;

  /** A client for `config`. Throws `config` if the client id is empty. */
  constructor(config: Config, options: AccountsOptions = {}) {
    if (!config.clientId) throw new AccountsError("config", "configuration: clientId is required");
    this.config = { ...config, issuer: config.issuer.replace(/\/+$/, "") };
    this.#fetch = options.fetch ?? ((input, init) => fetch(input, init));
    this.#keys = options.keys ? Keys.pinned(options.keys) : Keys.remote(`${this.config.issuer}/jwks`, this.#fetch);
  }

  /** Starts a sign-in: redirect the person to `url` and keep `pending` (see `startLogin`). */
  startLogin(options: LoginOptions = {}): Promise<StartedLogin> {
    return startLogin(this.config, options);
  }

  /**
   * Completes a sign-in from the callback's query: checks `state` and `iss`, exchanges the code, and verifies
   * the ID token (including the nonce).
   */
  async finishLogin(pending: PendingLogin, callbackQuery: string | URLSearchParams): Promise<SignedIn> {
    const callback = parseCallback(callbackQuery);
    const mismatch = "the sign-in callback doesn't match this browser's pending sign-in";
    if (callback.state !== pending.state) throw new AccountsError("state_mismatch", mismatch);
    if (callback.iss !== null && callback.iss !== this.config.issuer) {
      throw new AccountsError("state_mismatch", mismatch);
    }
    if (callback.error !== null) {
      throw new AccountsError("authorization", `sign-in failed: ${callback.error}`, {
        error: callback.error,
        description: callback.errorDescription ?? undefined,
      });
    }
    if (callback.code === null) throw new AccountsError("missing_code", "the callback has no code");
    const tokens = await this.#tokenRequest({
      grant_type: "authorization_code",
      code: callback.code,
      redirect_uri: this.config.redirectUri,
      code_verifier: pending.verifier,
    });
    const claims = await this.verifyIdToken(tokens.idToken, pending.nonce);
    return { claims, tokens, returnTo: pending.returnTo };
  }

  /**
   * Swaps a refresh token for new tokens. Store the new refresh token. A `token_refused` error
   * (`invalid_grant`) means it's expired, reused or the person removed your app: sign them in again.
   */
  refresh(refreshToken: string): Promise<TokenSet> {
    return this.#tokenRequest({ grant_type: "refresh_token", refresh_token: refreshToken });
  }

  /** Revokes a refresh token (and its whole family), e.g. on sign-out. */
  async revoke(refreshToken: string): Promise<void> {
    const response = await this.#post(`${this.config.issuer}/token/revocation`, {
      token: refreshToken,
      token_type_hint: "refresh_token",
    });
    if (!response.ok) {
      throw new AccountsError("api", `accounts API returned ${response.status}`, { status: response.status });
    }
  }

  /**
   * Signs out in one call: revokes `refreshToken` (if you hold one) and builds the end-session URL. End your own
   * session first; then redirect to `url` to also sign the person out of the accounts service (every project in that
   * browser), or skip it to sign out of your app alone. A failed revoke doesn't stop sign-out: it comes back as
   * `revokeError`.
   */
  async signOut(refreshToken: string | null, options: LogoutOptions = {}): Promise<SignOut> {
    let revokeError: AccountsError | null = null;
    if (refreshToken !== null) {
      try {
        await this.revoke(refreshToken);
      } catch (error) {
        if (!(error instanceof AccountsError)) throw error;
        revokeError = error;
      }
    }
    return { url: endSessionUrl(this.config, options), revokeError };
  }

  /** Verifies an ID token: signature, ES256, issuer, audience (your client id), expiry, and the nonce if given. */
  async verifyIdToken(token: string, nonce?: string): Promise<IdClaims> {
    const claims = await this.#decode(token, null);
    if (nonce !== undefined && claims.nonce !== nonce) {
      throw new AccountsError("invalid_token", "token rejected: nonce mismatch");
    }
    return { ...claims, amr: Array.isArray(claims.amr) ? claims.amr : [] } as IdClaims;
  }

  /**
   * Verifies an access token sent to *your* API (for example by your native app): signature, `typ: at+jwt`,
   * issuer, audience = your client id, expiry. Tokens issued to other apps are refused.
   */
  async verifyAccessToken(token: string): Promise<AccessClaims> {
    return (await this.#decode(token, "at+jwt")) as AccessClaims;
  }

  /** The person's profile from `/me`. */
  userinfo(accessToken: string): Promise<Record<string, unknown>> {
    return this.call(accessToken, "GET", `${this.config.issuer}/me`) as Promise<Record<string, unknown>>;
  }

  /** The friends, events and inbox API for one person. */
  friends(accessToken: string): Friends {
    return new Friends(this, accessToken);
  }

  /**
   * Reports someone for a guest, authenticated with your client credentials (so the config needs its client
   * secret). Fails with 400 for a bad reason or note, 404 for an unknown account or a guest, 429 for too many.
   */
  async reportAsGuest(report: GuestReport): Promise<void> {
    if (this.config.clientSecret === null) {
      throw new AccountsError("config", "reporting for a guest needs the client secret: set `clientSecret`");
    }
    const url = `${this.config.issuer.replace(/\/oidc$/, "")}/api/v1/reports`;
    await this.#callWith(`Basic ${this.#basic(this.config.clientSecret)}`, "POST", url, reportBody(report));
  }

  /**
   * Sends a notification as your project (your client credentials, so the config needs its client secret). The
   * service records it and answers at once: a send to one player is delivered inline (`result`); a fan-out
   * (`friendsOf`, `allPlayers`) is `queued`, so poll `notificationStatus` for its counts.
   *
   * Always set `idempotencyKey`: it is what makes a retry safe. With a key this call retries an intermittent
   * failure itself; without one it sends exactly once, since a repeat could notify twice. There are no caps and
   * no quiet hours; `collapseKey` replaces that player's unread item with the same key.
   *
   * Fails with `not_found` (404) when the named player never signed in to your project, `unprocessable` (422)
   * when the same idempotency key was used for a different request, `rate_limited` (429: 6000 requests an hour,
   * 10 fan-outs an hour), `api` for a bad request (400), or `config` without a client secret.
   */
  async sendNotification(notification: Notification): Promise<Receipt> {
    const keyed = notification.idempotencyKey !== undefined;
    return readReceipt(await this.#projectCall("POST", "/notifications", notificationBody(notification), keyed));
  }

  /**
   * Removes scheduled notifications with `collapseKey` that haven't gone out ("they played, so no 8 PM reminder"):
   * one player's (`user`, a `sub`), or all of your project's. Resolves with how many were removed.
   */
  async cancelNotifications(collapseKey: string, user?: string): Promise<number> {
    const body: Record<string, string> = { collapse_key: collapseKey };
    if (user !== undefined) body.user = user;
    const answer = await this.#projectCall("POST", "/notifications/cancel", body, false) as { cancelled?: unknown };
    if (typeof answer?.cancelled !== "number") {
      throw new AccountsError("api", "accounts API returned 200: missing field `cancelled`", { status: 200 });
    }
    return answer.cancelled;
  }

  /** Your project's own send, by `Receipt.id`, with its counts so far. Finished sends are kept 7 days. Fails with `not_found` for an unknown id or another project's. */
  async notificationStatus(id: string): Promise<Receipt> {
    return readReceipt(await this.#projectCall("GET", `/notifications/${encodeURIComponent(id)}`));
  }

  async #projectCall(method: string, path: string, body?: unknown, retryWrites = false): Promise<unknown> {
    if (this.config.clientSecret === null) {
      throw new AccountsError("config", "notifications need the client secret: set `clientSecret`");
    }
    const url = `${this.config.issuer.replace(/\/oidc$/, "")}/api/v1${path}`;
    return await this.#callWith(`Basic ${this.#basic(this.config.clientSecret)}`, method, url, body, {
      typedErrors: true,
      retryWrites,
    });
  }

  /** One authenticated JSON call; errors carry the API's message. Used by `Friends`. */
  call(token: string, method: string, url: string, body?: unknown): Promise<unknown> {
    return this.#callWith(`Bearer ${token}`, method, url, body);
  }

  async #callWith(
    authorization: string,
    method: string,
    url: string,
    body?: unknown,
    options: { typedErrors?: boolean; retryWrites?: boolean } = {},
  ): Promise<unknown> {
    const headers: Record<string, string> = {
      authorization,
      "user-agent": `toddstack-accounts/${VERSION}`,
    };
    if (body !== undefined) headers["content-type"] = "application/json";
    const response = await this.#send(
      url,
      { method, headers, body: body === undefined ? undefined : JSON.stringify(body) },
      options.retryWrites ? KEYED_SEND : READ,
      options.retryWrites,
    );
    const text = await response.text();
    if (!response.ok) {
      let message = text;
      try {
        const parsed = JSON.parse(text);
        if (typeof parsed?.error === "string") message = parsed.error;
      } catch { /* not JSON: keep the text */ }
      const kind = options.typedErrors ? TYPED_STATUS[response.status] ?? "api" : "api";
      throw new AccountsError(kind, `accounts API returned ${response.status}: ${message}`, {
        status: response.status,
      });
    }
    if (text === "") return null;
    try {
      return JSON.parse(text);
    } catch (cause) {
      throw new AccountsError("api", `accounts API returned ${response.status}: ${cause}`, {
        status: response.status,
        cause,
      });
    }
  }

  /** A GET is retried on an intermittent failure; anything else (token, refresh, revocation, writes) is sent once, unless `retryWrites`. */
  async #send(url: string, init: RequestInit, policy: RetryPolicy = READ, retryWrites = false): Promise<Response> {
    try {
      return await send(this.#fetch, url, init, policy, retryWrites);
    } catch (cause) {
      throw new AccountsError("http", `http: ${cause}`, { cause });
    }
  }

  /** RFC 6749 2.3.1 client credentials: form-encode both parts before Basic. */
  #basic(secret: string): string {
    const enc = (s: string) => new URLSearchParams({ v: s }).toString().slice(2);
    return btoa(`${enc(this.config.clientId)}:${enc(secret)}`);
  }

  /** A form POST with client authentication (Basic for confidential clients, `client_id` for public ones). */
  #post(url: string, params: Record<string, string>): Promise<Response> {
    const form = new URLSearchParams(params);
    const headers: Record<string, string> = {
      "content-type": "application/x-www-form-urlencoded",
      "user-agent": `toddstack-accounts/${VERSION}`,
    };
    if (this.config.clientSecret !== null) {
      headers.authorization = `Basic ${this.#basic(this.config.clientSecret)}`;
    } else {
      form.append("client_id", this.config.clientId);
    }
    return this.#send(url, { method: "POST", headers, body: form.toString() });
  }

  async #tokenRequest(params: Record<string, string>): Promise<TokenSet> {
    const response = await this.#post(`${this.config.issuer}/token`, params);
    if (!response.ok) {
      let error = `http_${response.status}`;
      let description: string | undefined;
      try {
        const body = await response.json();
        if (typeof body?.error === "string") {
          error = body.error;
          description = typeof body.error_description === "string" ? body.error_description : undefined;
        }
      } catch { /* not JSON */ }
      throw new AccountsError("token_refused", `token request refused: ${error}`, { error, description });
    }
    const raw = (await response.json()) as RawTokenSet;
    return {
      accessToken: raw.access_token,
      idToken: raw.id_token,
      refreshToken: raw.refresh_token ?? null,
      expiresIn: raw.expires_in,
      scope: raw.scope,
    };
  }

  async #decode(token: string, typ: string | null): Promise<Record<string, unknown>> {
    const reject = (why: string) => new AccountsError("invalid_token", `token rejected: ${why}`);
    const parts = token.split(".");
    if (parts.length !== 3) throw reject("malformed token");
    let header: { alg?: string; typ?: string; kid?: string };
    let claims: Record<string, unknown>;
    let signature: Uint8Array<ArrayBuffer>;
    try {
      header = JSON.parse(fromUtf8(fromBase64(parts[0])));
      claims = JSON.parse(fromUtf8(fromBase64(parts[1])));
      signature = fromBase64(parts[2]);
    } catch {
      throw reject("malformed token");
    }
    if (header.alg !== "ES256") throw reject("unexpected algorithm");
    if (typ !== null && header.typ?.toLowerCase() !== typ) throw reject("not an access token");
    if (!header.kid) throw reject("no kid");
    const key = await this.#keys.get(header.kid);
    const ok = await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      key,
      signature,
      utf8(`${parts[0]}.${parts[1]}`),
    );
    if (!ok) throw reject("InvalidSignature");
    if (claims.iss !== this.config.issuer) throw reject("InvalidIssuer");
    const aud = claims.aud;
    if (!(aud === this.config.clientId || (Array.isArray(aud) && aud.includes(this.config.clientId)))) {
      throw reject("InvalidAudience");
    }
    if (typeof claims.exp !== "number") throw reject("Missing required claim: exp");
    if (claims.exp < Math.floor(Date.now() / 1000) - LEEWAY_SECONDS) throw reject("ExpiredSignature");
    return claims;
  }
}
