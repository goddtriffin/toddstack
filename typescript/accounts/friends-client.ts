// In-app friend requests for the browser (or any client of your own server): calls the routes the Rust crate's
// `friends_router` mounts. Unlike `Friends`, it never sees an accounts token: your server holds that.

import { isValidId } from "./friends.ts";
import type { Blocks, Person, Relationship, Requests, Sent } from "./friends.ts";

/** Why a friends call failed, as the server's `error` code (`account_required`, `refused`, `rate_limited`, ...). */
export class FriendsClientError extends Error {
  /** HTTP status; 0 when the request never got an answer. */
  readonly status: number;
  /**
   * The server's error code: `account_required` (guest: offer sign-in), `session_expired`, `cross_site`,
   * `missing_scope`, `bad_request`, `refused` (not found, blocked, a privacy setting or a limit: `message` is
   * meant for the person), `not_found`, `rate_limited`, `accounts_unavailable`, or `network`.
   */
  readonly code: string;

  /** Builds an error from the HTTP status, the server's error code and its message. */
  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "FriendsClientError";
    this.status = status;
    this.code = code;
  }
}

/** Options for {@link FriendsClient}. */
export interface FriendsClientOptions {
  /** Where the routes live. Default `/api/v1/friends` (same origin). */
  base?: string;
  /** For tests, or a non-browser runtime. Default `globalThis.fetch`. */
  fetch?: typeof fetch;
  /**
   * Whether the person has a session. When given and false, calls throw `account_required` without a request
   * (the same behavior as the Swift and Kotlin clients with no stored session). Omit it to let the server decide.
   */
  signedIn?: () => boolean | Promise<boolean>;
  /** Extra headers per call, e.g. `Authorization: Bearer <app token>` from a non-cookie client. */
  headers?: () => HeadersInit | Promise<HeadersInit>;
}

/** The friends API of your own server: list, send, accept, decline, cancel, remove, block and unblock. */
export class FriendsClient {
  readonly #base: string;
  readonly #fetch: typeof fetch;
  readonly #signedIn?: () => boolean | Promise<boolean>;
  readonly #headers?: () => HeadersInit | Promise<HeadersInit>;

  /** Defaults to same-origin `/api/v1/friends` with the browser's cookies. */
  constructor(options: FriendsClientOptions = {}) {
    this.#base = (options.base ?? "/api/v1/friends").replace(/\/+$/, "");
    this.#fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.#headers = options.headers;
    this.#signedIn = options.signedIn;
  }

  static #id(value: string): string {
    if (!isValidId(value)) throw new FriendsClientError(400, "bad_request", "That isn't a valid person id.");
    return encodeURIComponent(value);
  }

  async #call<T>(method: string, path: string, body?: unknown): Promise<T> {
    if (this.#signedIn && !(await this.#signedIn())) {
      throw new FriendsClientError(401, "account_required", "Sign in to use friends.");
    }
    const headers = new Headers(this.#headers ? await this.#headers() : undefined);
    if (body !== undefined) headers.set("content-type", "application/json");
    let response: Response;
    try {
      response = await this.#fetch(`${this.#base}${path}`, {
        method,
        headers,
        credentials: "same-origin",
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (cause) {
      throw new FriendsClientError(0, "network", cause instanceof Error ? cause.message : String(cause));
    }
    const text: string = await response.text();
    let parsed: unknown = undefined;
    try {
      parsed = text === "" ? undefined : JSON.parse(text);
    } catch {
      // A non-JSON body (a proxy's error page) is reported below by status.
    }
    if (!response.ok) {
      const failure = (parsed ?? {}) as { error?: string; message?: string };
      throw new FriendsClientError(
        response.status,
        failure.error ?? "accounts_unavailable",
        failure.message ?? `Friends request failed (${response.status}).`,
      );
    }
    return parsed as T;
  }

  /** Friends who also use your app. */
  async list(): Promise<Person[]> {
    return (await this.#call<{ friends: Person[] }>("GET", "")).friends;
  }

  /** Pending requests, incoming and outgoing. */
  requests(): Promise<Requests> {
    return this.#call("GET", "/requests");
  }

  /** Sends a request by exact username. `accepted` means they had already asked. */
  async requestByUsername(username: string): Promise<Sent> {
    return (await this.#call<{ result: Sent }>("POST", "/requests", { username })).result;
  }

  /** Sends a request by friend code (works even when the person only takes requests with their code). */
  async requestByCode(friendCode: string): Promise<Sent> {
    return (await this.#call<{ result: Sent }>("POST", "/requests", { friend_code: friendCode })).result;
  }

  /** Accepts a request the person received. */
  async accept(sub: string): Promise<void> {
    await this.#call("POST", `/requests/${FriendsClient.#id(sub)}/accept`);
  }

  /** Declines a request the person received. */
  async decline(sub: string): Promise<void> {
    await this.#call("POST", `/requests/${FriendsClient.#id(sub)}/decline`);
  }

  /** Takes back a request the person sent. */
  async cancel(sub: string): Promise<void> {
    await this.#call("DELETE", `/requests/${FriendsClient.#id(sub)}`);
  }

  /** Ends a friendship. */
  async remove(sub: string): Promise<void> {
    await this.#call("DELETE", `/${FriendsClient.#id(sub)}`);
  }

  /** Blocks: ends the friendship and requests, both ways. */
  async block(sub: string): Promise<void> {
    await this.#call("POST", "/blocks", { sub: FriendsClient.#id(sub) });
  }

  /** Undoes {@link FriendsClient.block}. */
  async unblock(sub: string): Promise<void> {
    await this.#call("DELETE", `/blocks/${FriendsClient.#id(sub)}`);
  }

  /** Who the person blocked, and everyone to keep apart from them. */
  blocks(): Promise<Blocks> {
    return this.#call("GET", "/blocks");
  }

  /** Where the person stands with each of `subs` (1 to 100), keyed by sub: what a friend button should say. */
  async relationships(subs: string[]): Promise<Record<string, Relationship>> {
    const query: string = encodeURIComponent(subs.map((sub) => FriendsClient.#id(sub)).join(","));
    return (await this.#call<{ relationships: Record<string, Relationship> }>("GET", `/relationships?subs=${query}`))
      .relationships;
  }
}
