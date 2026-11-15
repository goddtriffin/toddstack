// Links from your project to the accounts site, tagged with your client id.

import type { Config } from "./tokens.ts";

/**
 * `path` on the accounts site (the issuer's origin), tagged with your client:
 * `accountUrl(config, "/account")` is `https://accounts.toddgriffin.me/account?client=<client id>`. Every link
 * to the accounts site carries it (the canonical pattern): the account pages then show "Back to <your project>",
 * and Todd can see which project sent people there. A query already in `path` is kept; a fragment stays last.
 */
export function accountUrl(config: Config, path: string = "/account"): string {
  const issuer = config.issuer.replace(/\/+$/, "");
  const origin = issuer.endsWith("/oidc") ? issuer.slice(0, -"/oidc".length) : issuer;
  const hash = path.indexOf("#");
  const fragment = hash >= 0 ? path.slice(hash) : "";
  const bare = hash >= 0 ? path.slice(0, hash) : path;
  const slash = bare.startsWith("/") ? "" : "/";
  const join = bare.includes("?") ? "&" : "?";
  return `${origin}${slash}${bare}${join}client=${encodeURIComponent(config.clientId)}${fragment}`;
}
