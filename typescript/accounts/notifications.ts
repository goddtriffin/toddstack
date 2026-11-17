import { AccountsError } from "./error.ts";

// Notifications: what your server sends to players, and the wire shapes for them. `accounts.sendNotification`,
// `accounts.cancelNotifications` and `accounts.notificationStatus` authenticate as your project (HTTP Basic, a
// confidential client), with no person signed in. `friends.setTimeZone` uses a person's access token instead.
//
// The rules, so your code can lean on them:
//
// - Always pass an `idempotencyKey`. A retry after a timeout then gets the recorded send back (`replayed`) instead
//   of sending twice. This library retries a send itself only when a key is set.
// - There are no caps and no quiet hours: every notification is delivered, one per friend who finishes a daily,
//   say. The phone's own Focus handles quiet time, so a streak warning is never delayed. Only a runaway guard
//   (over 100 to one player in an hour) drops the rest, counted as `droppedRunaway`.
// - `collapseKey` replaces that player's unread item with the same key from your project, and names a scheduled
//   notification so `cancelNotifications` can remove it. Nothing collapses without a key.

/**
 * Who a notification is for: `{ user }` one player (a `sub`) who has signed in to your project, `{ friendsOf }` each
 * friend of that player who uses your project and isn't blocked either way (shown as from that player), or
 * `{ allPlayers: true }` everyone who has signed in to your project.
 */
export type Recipient = { user: string } | { friendsOf: string } | { allPlayers: true };

/** A notification to send with `accounts.sendNotification`. */
export interface Notification {
  /** Who it is for. */
  recipient: Recipient;
  /** 1-40 of `a-z`, `0-9`, `_`; free-form, the first use creates it (at most 50 per project). Players can mute a type. */
  type: string;
  /** 1-80 characters. */
  title: string;
  /** At most 280 characters. */
  body?: string;
  /** An `https` link on an origin of your registered redirect or logout URIs. */
  url?: string;
  /** `HH:MM` on the player's own clock: the next time it reads that. Needs their time zone (`friends.setTimeZone`); UTC if unknown. */
  sendAtLocal?: string;
  /** 1-64 characters. Replaces the player's unread item with this key, and replaces or lets you cancel a scheduled one. */
  collapseKey?: string;
  /** 1-100 characters, kept 7 days. Always set one (a date or a game id). */
  idempotencyKey?: string;
  /**
   * The `sub` of the person the push is from (a uuid string). The service shows that person's name and avatar on the
   * push only to their friends, and only when neither side is a review account, is suspended, or has blocked the
   * other. Otherwise the push goes out plain, with no sender.
   */
  senderSub?: string;
}

/** What happened to one player: `not_delivered` folds together muted, left your project and suspended, so a send can't probe players. */
export type Outcome = "delivered" | "scheduled" | "not_delivered" | "dropped_runaway" | "unknown";

/** Outcomes, counted. */
export interface Counts {
  /** In the player's inbox now. */
  delivered: number;
  /** Waiting for their `sendAtLocal`. */
  scheduled: number;
  /** Muted, left your project, or suspended. */
  notDelivered: number;
  /** Dropped by the runaway guard. */
  droppedRunaway: number;
}

/** Where a send stands: `queued` is a fan-out still being delivered, `done` means every player was handled, `failed` is not retried. */
export type SendStatus = "queued" | "done" | "failed" | "unknown";

/** The service's answer to a send (202), and to `accounts.notificationStatus`. */
export interface Receipt {
  /** The send's id, for `accounts.notificationStatus`. */
  id: string;
  /** Where the send stands. */
  status: SendStatus;
  /** How many players it was addressed to. */
  recipients: number;
  /** Outcomes so far. */
  counts: Counts;
  /** For a send to one `user`: what happened to them. */
  result: Outcome | null;
  /** True when your idempotency key matched a recorded send: this is its answer, and nothing was sent again. */
  replayed: boolean;
}

/** The JSON body for `POST /api/v1/notifications`, leaving out what wasn't given. */
export function notificationBody(n: Notification): Record<string, unknown> {
  const recipient = "user" in n.recipient
    ? { user: n.recipient.user }
    : "friendsOf" in n.recipient
    ? { friends_of: n.recipient.friendsOf }
    : { all_players: true };
  const wire: Record<string, unknown> = { recipient, type: n.type, title: n.title, body: n.body ?? "" };
  if (n.url !== undefined) wire.url = n.url;
  if (n.sendAtLocal !== undefined) wire.send_at_local = n.sendAtLocal;
  if (n.collapseKey !== undefined) wire.collapse_key = n.collapseKey;
  if (n.idempotencyKey !== undefined) wire.idempotency_key = n.idempotencyKey;
  if (n.senderSub !== undefined) wire.sender_sub = n.senderSub;
  return wire;
}

/** Reads `counts` as the service sends it; a missing count is 0. */
export function readCounts(raw: unknown): Counts {
  const c = (raw ?? {}) as Record<string, unknown>;
  const n = (v: unknown) => typeof v === "number" ? v : 0;
  return {
    delivered: n(c.delivered),
    scheduled: n(c.scheduled),
    notDelivered: n(c.not_delivered),
    droppedRunaway: n(c.dropped_runaway),
  };
}

const OUTCOMES: readonly string[] = ["delivered", "scheduled", "not_delivered", "dropped_runaway"];
const STATUSES: readonly string[] = ["queued", "done", "failed"];

/** Reads a send's answer (the wire JSON) as a `Receipt`; a status or result newer than this library is `"unknown"`. Throws an `api` error without an `id`. */
export function readReceipt(raw: unknown): Receipt {
  const r = (raw ?? {}) as Record<string, unknown>;
  if (typeof r.id !== "string") {
    throw new AccountsError("api", "accounts API returned 200: missing field `id`", { status: 200 });
  }
  return {
    id: r.id,
    status: typeof r.status === "string" && STATUSES.includes(r.status) ? r.status as SendStatus : "unknown",
    recipients: typeof r.recipients === "number" ? r.recipients : 0,
    counts: readCounts(r.counts),
    result: typeof r.result === "string" ? OUTCOMES.includes(r.result) ? r.result as Outcome : "unknown" : null,
    replayed: r.replayed === true,
  };
}
