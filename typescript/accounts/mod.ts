/**
 * Client for Todd Griffin's Projects accounts (`accounts.toddgriffin.me`): what a project's backend needs to
 * sign people in, check tokens (locally, with instant logout), verify webhooks, act for players, and send notifications. The TypeScript half of `toddstack-accounts`
 * (same features as the Rust crate). Web Platform APIs only: runs on Node 20+, Deno and Bun.
 *
 * ```ts
 * import { Accounts, toddgriffin } from "@todd/toddstack-accounts";
 *
 * const accounts = new Accounts(toddgriffin("boggledygook", clientSecret, "https://www.boggledygook.com/auth/callback"));
 *
 * // GET /auth/login: redirect to `url`; keep `pending` server-side or in an encrypted HttpOnly cookie.
 * const { url, pending } = await accounts.startLogin({ scopes: ["profile", "offline_access"], returnTo: "/play" });
 *
 * // GET /auth/callback?code=..&state=..
 * const signedIn = await accounts.finishLogin(pending, new URL(request.url).search);
 * console.log(signedIn.claims.sub);
 * ```
 *
 * @module
 */

export type { Device, Environment, Platform } from "./devices.ts";
export { AccountsError, type AccountsErrorKind } from "./error.ts";
export {
  ALL_KINDS,
  type Blocks,
  type EventResult,
  type FriendEvent,
  Friends,
  type Inbox,
  type InboxItem,
  isValidId,
  MAX_RELATIONSHIPS_PER_CALL,
  type Person,
  type Relationship,
  type Requests,
  type Sent,
  type Unread,
} from "./friends.ts";
export { FriendsClient, FriendsClientError, type FriendsClientOptions } from "./friends-client.ts";
export { MockFriendsFetch, type MockReply, type SendArgs } from "./friends-client-mock.ts";
export type { Jwk, JwkSet } from "./jwks.ts";
export type { Counts, Notification, Outcome, Receipt, Recipient, SendStatus } from "./notifications.ts";
export { type GuestReport, MAX_NOTE_CHARS, type PlayerReport, type Reason } from "./report.ts";
export { MAX_IDEMPOTENCY_KEY_CHARS, Player, type Profile } from "./player.ts";
export { REVOCATION_POLL_INTERVAL_MS, type RevocationOptions, RevocationPoller, Revocations } from "./revocation.ts";
export {
  type Callback,
  type LoginOptions,
  parseCallback,
  type PendingLogin,
  pkceChallenge,
  type StartedLogin,
  startLogin,
} from "./login.ts";
export { endSessionUrl, type LogoutOptions, postLogoutTarget, type SignOut } from "./logout.ts";
export { accountUrl } from "./links.ts";
export { safeReturnTo } from "./return-to.ts";
export {
  type AccessClaims,
  Accounts,
  type AccountsOptions,
  type Config,
  hasScope,
  type IdClaims,
  ISSUER,
  LOCAL_BASE_URL,
  localConfig,
  type SignedIn,
  toddgriffin,
  toddgriffinAt,
  type TokenSet,
} from "./tokens.ts";
export { VERSION } from "./version.ts";
export { isReview, type Visibility, visibilityOf } from "./visibility.ts";
export {
  isKnownWebhookEventType,
  verifyWebhook,
  WEBHOOK_EVENT_TYPES,
  WEBHOOK_HEADER,
  WEBHOOK_TOLERANCE_SECONDS,
  type WebhookEvent,
  type WebhookEventType,
} from "./webhook.ts";
