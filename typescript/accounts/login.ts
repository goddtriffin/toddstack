// The sign-in round trip, as two steps around a redirect.
//
// 1. `startLogin` builds the URL to send the person to and a `PendingLogin`. Keep the pending login
//    server-side (or in an encrypted, HttpOnly cookie): it holds the PKCE verifier, which must never reach the
//    browser's JS.
// 2. When the person comes back to your redirect URI, pass the pending login and the callback query to
//    `Accounts.finishLogin`.

import { base64url, randomToken, utf8 } from "./encoding.ts";
import { safeReturnTo } from "./return-to.ts";
import type { Config } from "./tokens.ts";

/** What to remember between `startLogin` and the callback. Plain JSON: safe to serialize. */
export interface PendingLogin {
  /** Echoed back in the callback; must match. */
  state: string;
  /** Must come back inside the ID token. */
  nonce: string;
  /** The PKCE verifier. Never send it to the browser. */
  verifier: string;
  /** Where to go after sign-in: a path on your site, already checked. */
  returnTo: string | null;
}

/** Options beyond the defaults. */
export interface LoginOptions {
  /** Scopes to ask for; `openid` is always added. */
  scopes?: string[];
  /** A path on your site to return to; anything off-site is dropped. */
  returnTo?: string | null;
  /** `login` forces a fresh sign-in; `none` checks silently (no page shown); `create` opens the sign-up page. */
  prompt?: "login" | "none" | "create";
  /** Prefills the email field. */
  loginHint?: string;
  /** Require a sign-in within this many seconds. */
  maxAge?: number;
}

/** A started sign-in: redirect to `url`, keep `pending`. */
export interface StartedLogin {
  /** Where to send the person. */
  url: string;
  /** What to keep until the callback. */
  pending: PendingLogin;
}

/** The PKCE S256 challenge for `verifier`. */
export async function pkceChallenge(verifier: string): Promise<string> {
  return base64url(new Uint8Array(await crypto.subtle.digest("SHA-256", utf8(verifier))));
}

/** Starts a sign-in. */
export async function startLogin(config: Config, options: LoginOptions = {}): Promise<StartedLogin> {
  const pending: PendingLogin = {
    state: randomToken(),
    nonce: randomToken(),
    verifier: randomToken(),
    returnTo: options.returnTo ? safeReturnTo(options.returnTo) : null,
  };
  const scopes = [...(options.scopes ?? [])];
  if (!scopes.includes("openid")) scopes.unshift("openid");
  const url = new URL(`${config.issuer}/auth`);
  const q = url.searchParams;
  q.append("client_id", config.clientId);
  q.append("redirect_uri", config.redirectUri);
  q.append("response_type", "code");
  q.append("scope", scopes.join(" "));
  q.append("state", pending.state);
  q.append("nonce", pending.nonce);
  q.append("code_challenge", await pkceChallenge(pending.verifier));
  q.append("code_challenge_method", "S256");
  if (options.prompt) q.append("prompt", options.prompt);
  if (options.loginHint) q.append("login_hint", options.loginHint);
  if (options.maxAge !== undefined) q.append("max_age", String(options.maxAge));
  return { url: url.toString(), pending };
}

/** What the callback query says. */
export interface Callback {
  /** The authorization code. */
  code: string | null;
  /** The `state` to match against the pending sign-in. */
  state: string | null;
  /** The issuer (RFC 9207), when sent. */
  iss: string | null;
  /** An OAuth error code, when the sign-in failed. */
  error: string | null;
  /** The error's description. */
  errorDescription: string | null;
}

/** Reads a callback query string (with or without `?`) or `URLSearchParams`. */
export function parseCallback(query: string | URLSearchParams): Callback {
  const q = typeof query === "string" ? new URLSearchParams(query.replace(/^\?/, "")) : query;
  return {
    code: q.get("code"),
    state: q.get("state"),
    iss: q.get("iss"),
    error: q.get("error"),
    errorDescription: q.get("error_description"),
  };
}
