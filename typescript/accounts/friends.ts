// The friends, events and inbox API, called with a person's access token (scopes `friends`, and
// `friends:events` to post events). Your app only ever sees friends who also use your app.

import { AccountsError } from "./error.ts";
import { type Counts, readCounts } from "./notifications.ts";
import { type Reason, reportBody } from "./report.ts";
import type { Accounts } from "./tokens.ts";

/** A friend, request or blocked person as your app sees them. */
export interface Person {
  /** Their permanent id. */
  sub: string;
  /** Their unique username. */
  username: string;
  /** Display name, if set. */
  name: string | null;
  /** Profile picture URL, if set. */
  picture: string | null;
  /** When the friendship, request or block started (RFC 3339). */
  since: string;
}

/** Pending friend requests. */
export interface Requests {
  /** Requests sent to the person. */
  incoming: Person[];
  /** Requests the person sent. */
  outgoing: Person[];
}

/** The person's blocks, for keeping people apart in your app. */
export interface Blocks {
  /** People this person blocked, newest first: for an Unblock list. */
  blocked: Person[];
  /**
   * Everyone blocked either way (sorted `sub`s). Hide these from the person in friends lists, "Playing now"
   * and chat, and them from the person, but never show it as a block (blocks are silent) and don't filter
   * leaderboards.
   */
  hidden: string[];
}

/** What sending a request did: `accepted` means they had already asked, so you're friends now. */
export type Sent = "requested" | "accepted" | "already_friends";

/**
 * Where the person stands with someone: what a friend button should say. `friends` is "Friends",
 * `request_sent` is "Requested" (`cancel` takes it back), `request_received` is "Accept" (`accept`),
 * `blocked` (the person blocked them) is "Unblock", and `none` is "Add friend". `none` also covers the
 * person themselves, unknown or suspended accounts, people who don't use your app, and someone who blocked
 * the person, so a block can't be probed.
 */
export type Relationship = "friends" | "request_sent" | "request_received" | "blocked" | "none";

/** The service answers at most this many people per call; `relationships` splits longer lists. */
export const MAX_RELATIONSHIPS_PER_CALL = 100;

/** An event for friends' inboxes ("Alex beat your high score"). */
export interface FriendEvent {
  /** 1-40 of `a-z`, `0-9`, `_`. People can mute a kind. */
  kind: string;
  /** 1-80 characters. */
  title: string;
  /** Up to 280 characters. */
  body?: string;
  /** An `https` link into your app. */
  url?: string;
  /** A newer event with the same key replaces an unread older one. */
  collapseKey?: string;
  /** Only these friends (`sub`s); all friends who use your app if omitted. */
  recipients?: string[];
}

/** One item in the person's inbox from your app. */
export interface InboxItem {
  /** Item id (for `markRead`). */
  id: string;
  /**
   * The project the item belongs to; null for an account-level item from the service itself ("Alex accepted your
   * friend request"): shown in every project, one item, read everywhere at once, never muted or dropped.
   */
  clientId: string | null;
  /** The friend it came from (`sub`), null for system items. */
  from: string | null;
  /** That friend's username. */
  fromUsername: string | null;
  /** The event kind. */
  kind: string;
  /** The headline. */
  title: string;
  /** The text (may be empty). */
  body: string;
  /** An `https` link into your app. */
  url: string | null;
  /** When it was posted (RFC 3339). */
  createdAt: string;
  /** When the person read it (RFC 3339), null if unread. */
  readAt: string | null;
}

/** Unread inbox counts. */
export interface Unread {
  /** Unread items from your project. */
  project: number;
  /** Unread account-level items, shared by every project. */
  account: number;
  /** Both together: the badge number. */
  total: number;
}

/** The person's inbox for your project (its items plus the account-level ones) and the unread counts. */
export interface Inbox {
  /** Newest first. */
  items: InboxItem[];
  /** Null from a service older than the counts. */
  unread: Unread | null;
}

/** What posting an event did. */
export interface EventResult {
  /** How many friends' inboxes it reached. */
  delivered: number;
  /** Dropped by the runaway guard (over 100 to one player in an hour). */
  dropped: number;
  /** Every outcome, counted; null from a service older than the counts. */
  counts: Counts | null;
}

/**
 * Whether `value` is safe to put in a URL path: 1 to 64 of `A-Za-z0-9_-`. A real `sub` is a UUID; anything else
 * (`/`, `?`, `#`, `.`, `%`) could redirect the call to another accounts path carrying the person's token.
 */
export function isValidId(value: string): boolean {
  return /^[A-Za-z0-9_-]{1,64}$/.test(value);
}

function checked(value: string): string {
  if (!isValidId(value)) throw new AccountsError("api", "not a valid id", { status: 400 });
  return value;
}

/** Mutes every event kind from your app. */
export const ALL_KINDS = "*";

interface RawInboxItem {
  id: string;
  client_id?: string | null;
  from: string | null;
  from_username: string | null;
  kind: string;
  title: string;
  body: string;
  url: string | null;
  created_at: string;
  read_at: string | null;
}

function eventBody(event: FriendEvent): Record<string, unknown> {
  const wire: Record<string, unknown> = { kind: event.kind, title: event.title, body: event.body ?? "" };
  if (event.url !== undefined) wire.url = event.url;
  if (event.collapseKey !== undefined) wire.collapse_key = event.collapseKey;
  if (event.recipients !== undefined) wire.recipients = event.recipients;
  return wire;
}

function field<T>(value: unknown, name: string): T {
  const out = (value as Record<string, unknown> | null)?.[name];
  if (out === undefined) {
    throw new AccountsError("api", `accounts API returned 200: missing field \`${name}\``, { status: 200 });
  }
  return out as T;
}

function readItem(i: RawInboxItem): InboxItem {
  return {
    id: i.id,
    clientId: i.client_id ?? null,
    from: i.from,
    fromUsername: i.from_username,
    kind: i.kind,
    title: i.title,
    body: i.body,
    url: i.url,
    createdAt: i.created_at,
    readAt: i.read_at,
  };
}

/** The API for one person (their access token). Get one from `accounts.friends(token)`. */
export class Friends {
  #accounts: Accounts;
  #token: string;
  #base: string;

  /** The API for the person whose access token this is. */
  constructor(accounts: Accounts, accessToken: string) {
    this.#accounts = accounts;
    this.#token = accessToken;
    this.#base = `${accounts.config.issuer.replace(/\/oidc$/, "")}/api/v1`;
  }

  #call(method: string, path: string, body?: unknown): Promise<unknown> {
    return this.#accounts.call(this.#token, method, `${this.#base}${path}`, body);
  }

  /** Friends who also use your app. */
  async list(): Promise<Person[]> {
    return field(await this.#call("GET", "/friends"), "friends");
  }

  /** Incoming and outgoing requests. */
  async requests(): Promise<Requests> {
    const value = await this.#call("GET", "/friends/requests");
    return { incoming: field(value, "incoming"), outgoing: field(value, "outgoing") };
  }

  /** Where the person stands with each of `subs`, keyed by sub. Any number: longer lists go out as several calls. */
  async relationships(subs: string[]): Promise<Record<string, Relationship>> {
    subs.forEach(checked);
    const found: Record<string, Relationship> = {};
    for (let i = 0; i < subs.length; i += MAX_RELATIONSHIPS_PER_CALL) {
      const chunk = subs.slice(i, i + MAX_RELATIONSHIPS_PER_CALL);
      const value = await this.#call("GET", `/friends/relationships?subs=${encodeURIComponent(chunk.join(","))}`);
      Object.assign(found, field<Record<string, Relationship>>(value, "relationships"));
    }
    return found;
  }

  /** Where the person stands with `sub`. */
  async relationship(sub: string): Promise<Relationship> {
    return (await this.relationships([sub]))[sub] ?? "none";
  }

  /** Sends a request by exact username. Not found also covers blocks and privacy settings. */
  async requestByUsername(username: string): Promise<Sent> {
    return field(await this.#call("POST", "/friends/requests", { username }), "result");
  }

  /** Sends a request by friend code (works even when the person only takes requests from people with their code). */
  async requestByCode(friendCode: string): Promise<Sent> {
    return field(await this.#call("POST", "/friends/requests", { friend_code: friendCode }), "result");
  }

  /** Accepts `sub`'s request. */
  async accept(sub: string): Promise<void> {
    await this.#call("POST", `/friends/requests/${checked(sub)}/accept`);
  }

  /** Declines `sub`'s request. */
  async decline(sub: string): Promise<void> {
    await this.#call("POST", `/friends/requests/${checked(sub)}/decline`);
  }

  /** Cancels a request you sent. */
  async cancel(sub: string): Promise<void> {
    await this.#call("DELETE", `/friends/requests/${checked(sub)}`);
  }

  /** Ends a friendship. */
  async remove(sub: string): Promise<void> {
    await this.#call("DELETE", `/friends/${checked(sub)}`);
  }

  /** Blocks: ends the friendship and requests, and stops events both ways. */
  async block(sub: string): Promise<void> {
    await this.#call("POST", "/blocks", { sub: checked(sub) });
  }

  /** Undoes `block`. */
  async unblock(sub: string): Promise<void> {
    await this.#call("DELETE", `/blocks/${checked(sub)}`);
  }

  /** Who the person blocked, and everyone to keep apart from them. */
  async blocks(): Promise<Blocks> {
    const value = await this.#call("GET", "/blocks");
    return { blocked: field(value, "blocked"), hidden: field(value, "hidden") };
  }

  /**
   * Reports `sub` to Todd's review queue, from this person, at your app. Needs no particular scope. Fails with
   * 400 for reporting themselves or a bad note, 404 for an unknown account or a guest, 429 for too many.
   */
  async report(sub: string, reason: Reason, note?: string): Promise<void> {
    await this.#call("POST", "/reports", reportBody({ sub, reason, note }));
  }

  /** Posts an event; returns how many friends' inboxes it reached. Rate limit: 60 per person per hour per app. */
  async postEvent(event: FriendEvent): Promise<number> {
    return field(await this.#call("POST", "/events", eventBody(event)), "delivered");
  }

  /** Posts an event, like `postEvent`, and returns the whole answer: delivered and dropped, and the full counts. */
  async postEventResult(event: FriendEvent): Promise<EventResult> {
    const value = await this.#call("POST", "/events", eventBody(event)) as Record<string, unknown>;
    return {
      delivered: field(value, "delivered"),
      dropped: typeof value.dropped === "number" ? value.dropped : 0,
      counts: value.counts === undefined || value.counts === null ? null : readCounts(value.counts),
    };
  }

  /** The person's inbox items from your app, newest first, plus the account-level ones (`clientId` is null). */
  async inbox(limit: number): Promise<InboxItem[]> {
    return (await this.inboxWithUnread(limit)).items;
  }

  /** The inbox with the unread counts for your badge. */
  async inboxWithUnread(limit: number): Promise<Inbox> {
    const value = await this.#call("GET", `/inbox?limit=${Math.floor(limit)}`);
    const raw = (value as { unread?: Partial<Unread> | null }).unread;
    return {
      items: field<RawInboxItem[]>(value, "items").map(readItem),
      unread: raw ? { project: raw.project ?? 0, account: raw.account ?? 0, total: raw.total ?? 0 } : null,
    };
  }

  /**
   * Tells the service the person's time zone (an IANA name such as `America/Chicago`, from their browser or
   * device), so a notification's `sendAtLocal` lands on their clock. Needs no particular scope; the last write
   * wins. Send it when they sign in and when it changes. Fails with 400 for a name the service doesn't know.
   */
  async setTimeZone(timeZone: string): Promise<void> {
    await this.#call("PUT", "/me/time-zone", { time_zone: timeZone });
  }

  /** Marks an inbox item read. */
  async markRead(id: string): Promise<void> {
    await this.#call("POST", `/inbox/${checked(id)}/read`);
  }

  /** The event kinds from your app the person has muted (`ALL_KINDS` means all of them). */
  async mutes(): Promise<string[]> {
    return field(await this.#call("GET", "/mutes"), "mutes");
  }

  /** Stops friends' events of `kind` from your app reaching the person (`ALL_KINDS` for every kind). */
  async mute(kind: string): Promise<void> {
    await this.#call("PUT", `/mutes/${kind === ALL_KINDS ? kind : checked(kind)}`);
  }

  /** Undoes `mute`. */
  async unmute(kind: string): Promise<void> {
    await this.#call("DELETE", `/mutes/${kind === ALL_KINDS ? kind : checked(kind)}`);
  }
}
