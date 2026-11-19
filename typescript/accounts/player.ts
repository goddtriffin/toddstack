// Everything your server does for a signed-in player, as your project. `accounts.player(sub)` names a player by `sub`
// (one you authenticated with your own session) and returns a `Player` with a method for each route under
// `/api/v1/players/{sub}`. Calls authenticate with your client credentials (HTTP Basic, so the config needs its client
// secret); a player's access or refresh token is never involved, so your server holds none past the sign-in code
// exchange.
//
// - Errors carry the code: a refusal is an `AccountsError` of kind `server` with the API's `error` code in `code`
//   (`not_found`, `suspended`, `scope`, `rate_limited`, ...). Everything but a 429 and a 5xx is final.
// - Writes are idempotent, so this library retries them (3 attempts, on a 429, a 5xx or a network failure only): friend
//   requests, accepts, blocks, mutes and the rest are no-op successes when the state already holds, and an event is
//   deduplicated by its required idempotency key.
// - A player who never signed in to your project, a deleted one and one who removed your app are the same `not_found`.
//   A suspended player can be read, not written for (`suspended`).
// - You only ever see people who also use your project, in every list.

import { AccountsError } from "./error.ts";
import {
  ALL_KINDS,
  type Blocks,
  checked,
  eventBody,
  type EventResult,
  field,
  type FriendEvent,
  type Inbox,
  MAX_RELATIONSHIPS_PER_CALL,
  type Person,
  type RawInboxItem,
  readItem,
  type Relationship,
  type Requests,
  type Sent,
  type Unread,
} from "./friends.ts";
import { readCounts } from "./notifications.ts";
import type { Reason } from "./report.ts";
import type { Accounts } from "./tokens.ts";
import type { Visibility } from "./visibility.ts";

/** The most characters of an idempotency key the service accepts. */
export const MAX_IDEMPOTENCY_KEY_CHARS = 100;

/**
 * What `Player.profile` answers: the claims for the scopes in both the player's grant and your project's allowed
 * scopes, plus `visibility` and `suspended`. Fields behind a scope are absent without it.
 */
export interface Profile {
  /** The player's permanent id. */
  sub: string;
  /** With the `profile` scope. */
  username?: string;
  /** With the `profile` scope: the same as `username`. */
  preferred_username?: string;
  /** With the `profile` scope. */
  name?: string;
  /** With the `profile` scope. */
  picture?: string;
  /** With the `profile` scope: Unix seconds. */
  updated_at?: number;
  /** With the `email` scope. */
  email?: string;
  /** With the `email` scope. */
  email_verified?: boolean;
  /** `review` for a review account: never show it to others. */
  visibility: Visibility;
  /** Suspended players can be read but not written for: end their session. */
  suspended: boolean;
}

function serverError(code: string, message: string): AccountsError {
  return new AccountsError("server", `accounts API returned 400 ${code}: ${message}`, { status: 400, code });
}

/** A plain id, or a `server` `bad_request` error. */
export function checkedId(value: string): string {
  try {
    return checked(value);
  } catch {
    throw serverError("bad_request", "not a valid id");
  }
}

/** The Server API for one player. Make it with `accounts.player(sub)`. */
export class Player {
  #accounts: Accounts;
  /** The player's `sub`. */
  readonly sub: string;

  /** Use `accounts.player(sub)`. */
  constructor(accounts: Accounts, sub: string) {
    this.#accounts = accounts;
    this.sub = checkedId(sub);
  }

  #call(method: string, path: string, body?: unknown): Promise<unknown> {
    return this.#accounts.serverCall(method, `/players/${this.sub}${path}`, body);
  }

  /**
   * The player's claims for the scopes they granted and you may use, plus `visibility` and `suspended`. Replaces
   * `/oidc/me` after sign-in. `not_found` for a player who never signed in to your project.
   */
  async profile(): Promise<Profile> {
    const raw = await this.#call("GET", "") as Profile;
    return { ...raw, visibility: raw.visibility === "review" ? "review" : "normal", suspended: raw.suspended === true };
  }

  /** Friends who also use your project. Needs the `friends` scope (`scope` without it). */
  async friends(): Promise<Person[]> {
    return field(await this.#call("GET", "/friends"), "friends");
  }

  /** Where the player stands with each of `subs`, keyed by sub. Any number: longer lists go out as several calls. */
  async relationships(subs: readonly string[]): Promise<Record<string, Relationship>> {
    subs.forEach(checkedId);
    const found: Record<string, Relationship> = {};
    for (let i = 0; i < subs.length; i += MAX_RELATIONSHIPS_PER_CALL) {
      const chunk = subs.slice(i, i + MAX_RELATIONSHIPS_PER_CALL);
      const value = await this.#call("GET", `/friends/relationships?subs=${encodeURIComponent(chunk.join(","))}`);
      Object.assign(found, field<Record<string, Relationship>>(value, "relationships"));
    }
    return found;
  }

  /** Where the player stands with `sub`. */
  async relationship(sub: string): Promise<Relationship> {
    return (await this.relationships([sub]))[sub] ?? "none";
  }

  /** Requests with people who use your project. */
  async requests(): Promise<Requests> {
    const value = await this.#call("GET", "/friends/requests");
    return { incoming: field(value, "incoming"), outgoing: field(value, "outgoing") };
  }

  async #requestWith(body: Record<string, string>): Promise<Sent> {
    return field(await this.#call("POST", "/friends/requests", body), "result");
  }

  /**
   * Asks `sub` (any account holder, not only your players) to be friends. `accepted` means they had already asked.
   * `not_found` for anyone who can't be asked (unknown, blocked either way, private): the same answer for every cause.
   */
  request(sub: string): Promise<Sent> {
    return this.#requestWith({ sub: checkedId(sub) });
  }

  /** Asks by exact username. */
  requestByUsername(username: string): Promise<Sent> {
    return this.#requestWith({ username });
  }

  /** Asks by friend code (works even when the person only takes requests from people with their code). */
  requestByCode(friendCode: string): Promise<Sent> {
    return this.#requestWith({ friend_code: friendCode });
  }

  /** Accepts a request from `sub`, a player of your project. A no-op when already friends; `not_found` when there is neither. */
  async accept(sub: string): Promise<void> {
    await this.#call("POST", `/friends/requests/${checkedId(sub)}/accept`);
  }

  /** Declines a request from `sub`. A no-op when there is none. */
  async decline(sub: string): Promise<void> {
    await this.#call("POST", `/friends/requests/${checkedId(sub)}/decline`);
  }

  /** Takes back a request the player sent. A no-op when there is none. */
  async cancel(sub: string): Promise<void> {
    await this.#call("DELETE", `/friends/requests/${checkedId(sub)}`);
  }

  /** Ends a friendship. A no-op when not friends. */
  async remove(sub: string): Promise<void> {
    await this.#call("DELETE", `/friends/${checkedId(sub)}`);
  }

  /** Blocks `sub`, a player of your project: ends the friendship and requests, and stops events both ways. */
  async block(sub: string): Promise<void> {
    await this.#call("PUT", `/blocks/${checkedId(sub)}`);
  }

  /** Lifts a block, even on someone who doesn't use your project. A no-op when not blocked. */
  async unblock(sub: string): Promise<void> {
    await this.#call("DELETE", `/blocks/${checkedId(sub)}`);
  }

  /** Who the player blocked, and everyone to keep apart from them (people of your project only). */
  async blocks(): Promise<Blocks> {
    const value = await this.#call("GET", "/blocks");
    return { blocked: field(value, "blocked"), hidden: field(value, "hidden") };
  }

  /**
   * Posts an event to the player's friends who use your project. The `idempotencyKey` (1 to 100 characters, kept 7
   * days) is required: a retry with it answers the same (`replayed`) and sends nothing; the same key with a
   * different event is `unprocessable`. Use something stable for the occasion, like a game id. Needs `friends:events`.
   */
  async postEvent(event: FriendEvent, idempotencyKey: string): Promise<EventResult> {
    if (idempotencyKey.length === 0 || [...idempotencyKey].length > MAX_IDEMPOTENCY_KEY_CHARS) {
      throw serverError("bad_request", "idempotency_key is required: 1 to 100 characters.");
    }
    const value = await this.#call("POST", "/events", {
      ...eventBody(event),
      idempotency_key: idempotencyKey,
    }) as Record<string, unknown>;
    return {
      replayed: value.replayed === true,
      delivered: field(value, "delivered"),
      dropped: typeof value.dropped === "number" ? value.dropped : 0,
      counts: value.counts === undefined || value.counts === null ? null : readCounts(value.counts),
    };
  }

  /** Reports `sub` to Todd's review queue, from this player, at your project. `note` is at most `MAX_NOTE_CHARS`. */
  async report(sub: string, reason: Reason, note?: string): Promise<void> {
    await this.#accounts.reportAsPlayer({ reporterSub: this.sub, sub, reason, note });
  }

  /** The player's inbox: your project's items plus account-level ones, newest first, with the unread counts. */
  async inbox(limit: number): Promise<Inbox> {
    const value = await this.#call("GET", `/inbox?limit=${Math.floor(limit)}`);
    const raw = (value as { unread?: Partial<Unread> | null }).unread;
    return {
      items: field<RawInboxItem[]>(value, "items").map(readItem),
      unread: raw ? { project: raw.project ?? 0, account: raw.account ?? 0, total: raw.total ?? 0 } : null,
    };
  }

  /** Marks an inbox item read. Another project's item is a no-op. */
  async markRead(id: string): Promise<void> {
    await this.#call("POST", `/inbox/${checkedId(id)}/read`);
  }

  /** The event kinds from your project the player muted (`ALL_KINDS` means all of them). */
  async mutes(): Promise<string[]> {
    return field(await this.#call("GET", "/mutes"), "mutes");
  }

  /** Stops friends' events of `kind` reaching the player (`ALL_KINDS` for every kind). */
  async mute(kind: string): Promise<void> {
    await this.#call("PUT", `/mutes/${kind === ALL_KINDS ? kind : checkedId(kind)}`);
  }

  /** Undoes `mute`. */
  async unmute(kind: string): Promise<void> {
    await this.#call("DELETE", `/mutes/${kind === ALL_KINDS ? kind : checkedId(kind)}`);
  }

  /**
   * Tells the service the player's time zone (an IANA name such as `America/Chicago`), so a notification's
   * `send_at_local` lands on their clock. The last write wins.
   */
  async setTimeZone(timeZone: string): Promise<void> {
    await this.#call("PUT", "/time-zone", { time_zone: timeZone });
  }
}
