// Everything that can go wrong talking to the accounts service. One error class with a `kind`, the
// TypeScript shape of the Rust crate's `Error` enum.

/** What went wrong. */
export type AccountsErrorKind =
  /** The callback's `state` (or `iss`) doesn't match the pending sign-in: forged or stale. Start over. */
  | "state_mismatch"
  /** The service sent the person back with an OAuth error (`access_denied`, `login_required` for `prompt=none`). */
  | "authorization"
  /** The callback had no `code`. */
  | "missing_code"
  /** The token endpoint refused (`invalid_grant`: expired, reused or revoked). Sign in again. */
  | "token_refused"
  /** A token failed verification (signature, issuer, audience, expiry, nonce). */
  | "invalid_token"
  /** A webhook failed verification. */
  | "invalid_webhook"
  /** The API refused a call; `status` and the API's message are set. */
  | "api"
  /** A notification call named something that doesn't exist (404): a player who never signed in to your project, or an unknown send. `status` is set. */
  | "not_found"
  /** A device registration named a token that belongs to another project (409). `status` is set. */
  | "conflict"
  /** A notification call was refused as unprocessable (422): the same idempotency key with a different request, or a field the service can't parse. `status` is set. */
  | "unprocessable"
  /** A notification call hit a rate limit (429). Wait before sending again. `status` is set. */
  | "rate_limited"
  /**
   * A Server API call (`accounts.player(sub)`, `accounts.reportAsPlayer`) was refused: `status`, `code` (the API's `error`
   * field: `not_found`, `suspended`, `scope`, `rate_limited`, `bad_request`, `unprocessable`, ...) and the message are set.
   * Match on `code`. Everything but a 429 and a 5xx is final.
   */
  | "server"
  /** The request itself failed (network, timeout). */
  | "http"
  /** Bad configuration. */
  | "config";

/** An error from this library. Switch on `kind`. */
export class AccountsError extends Error {
  /** What went wrong. */
  readonly kind: AccountsErrorKind;
  /** The OAuth error code, for `authorization` and `token_refused`. */
  readonly error?: string;
  /** The OAuth `error_description`, when the service sent one. */
  readonly description?: string;
  /** The HTTP status, for `api`, `server`, `not_found`, `conflict`, `unprocessable` and `rate_limited`. */
  readonly status?: number;
  /** The API's `error` code, for `server`. */
  readonly code?: string;

  /** Builds an error; `details` carries the OAuth code, description, HTTP status or cause. */
  constructor(
    kind: AccountsErrorKind,
    message: string,
    details: { error?: string; description?: string; status?: number; code?: string; cause?: unknown } = {},
  ) {
    super(message, details.cause === undefined ? undefined : { cause: details.cause });
    this.name = "AccountsError";
    this.kind = kind;
    this.error = details.error;
    this.description = details.description;
    this.status = details.status;
    this.code = details.code;
  }
}
