// Signing out, as one call in your `/logout` route.
//
// Your session is your own: end it yourself. `Accounts.signOut` then revokes your refresh token and hands back the
// accounts service's end-session URL (RP-initiated logout). Redirect there only if the person should leave *every*
// project: the service asks "Sign out?" and, on yes, signs them out of all of Todd's projects in that browser. To
// sign out of your app alone, revoke and skip the redirect.

import type { AccountsError } from "./error.ts";
import { safeReturnTo } from "./return-to.ts";
import type { Config } from "./tokens.ts";

/** What to put on the end-session URL. */
export interface LogoutOptions {
  /** The ID token from sign-in (expired is fine). Sent as `id_token_hint`. */
  idTokenHint?: string;
  /**
   * Where the person lands afterwards: a path on your site (`/`, checked like `safeReturnTo`) or an absolute URL on
   * your redirect URI's origin. Anything else is dropped. The resulting URL must also be registered for your app as
   * a post-logout redirect URI, exactly, or the service ignores it and shows its own "You're signed out" page.
   */
  postLogoutRedirectUri?: string;
  /** Echoed back as `?state=` on that redirect. */
  state?: string;
}

/** What `Accounts.signOut` did. */
export interface SignOut {
  /** The end-session URL (see `endSessionUrl`). */
  url: string;
  /** Why revoking the refresh token failed, if it did. Sign-out goes ahead regardless; log this. */
  revokeError: AccountsError | null;
}

/** The absolute post-logout URL for `target`, if it stays on your site (the origin of `config.redirectUri`). */
export function postLogoutTarget(config: Config, target: string): string | null {
  let base: URL;
  let url: URL;
  try {
    base = new URL(config.redirectUri);
    const path = safeReturnTo(target);
    url = path !== null ? new URL(path, base) : new URL(target);
  } catch {
    return null;
  }
  const sameSite = url.origin === base.origin && url.username === "" && url.password === "" && url.hash === "" &&
    !target.includes("#");
  return sameSite ? url.toString() : null;
}

/**
 * The end-session URL. Always carries your `client_id`, so the service recognizes your app (and your post-logout
 * URI) even without an ID token, or with one signed by a key it has since retired.
 */
export function endSessionUrl(config: Config, options: LogoutOptions = {}): string {
  const url = new URL(`${config.issuer}/session/end`);
  const q = url.searchParams;
  q.append("client_id", config.clientId);
  if (options.idTokenHint !== undefined) q.append("id_token_hint", options.idTokenHint);
  const target = options.postLogoutRedirectUri === undefined
    ? null
    : postLogoutTarget(config, options.postLogoutRedirectUri);
  if (target !== null) q.append("post_logout_redirect_uri", target);
  if (options.state !== undefined) q.append("state", options.state);
  return url.toString();
}
