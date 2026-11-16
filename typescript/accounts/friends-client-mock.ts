// A mock `fetch` for `FriendsClient` (mock design: arguments recorded, responses seeded per exact request and
// consumed in FIFO order, an unseeded call throws, `assertAllSeedsConsumed` checks nothing was left over).

/** The arguments of one call: method, path under the client's base (`/api/v1/friends/...`) and raw JSON body. */
export interface SendArgs {
  /** HTTP method. */
  method: string;
  /** Path and query under the server. */
  path: string;
  /** The raw JSON body, if any. */
  body?: string;
}

/** A seeded answer: an HTTP status and JSON body, or an `Error` for a request that failed (network). */
export type MockReply = { status: number; body?: unknown } | Error;

/**
 * Pass `mock.fetch` as `FriendsClient`'s `fetch` option. Seed a reply per exact call with `seed`. A call with no seed
 * throws `mock_no_response_configured`: it is a bug in the test. End every test that seeds with `assertAllSeedsConsumed`.
 */
export class MockFriendsFetch {
  readonly #seeds: { args: SendArgs; replies: MockReply[] }[] = [];
  readonly #calls: SendArgs[] = [];

  /** Every call received, in order. */
  get calls(): readonly SendArgs[] {
    return this.#calls;
  }

  /** Seeds the answer to the call with exactly these arguments; the same call seeded twice answers in order. */
  seed(args: SendArgs, reply: MockReply): void {
    const found = this.#seeds.find((s) => sameArgs(s.args, args));
    if (found) found.replies.push(reply);
    else this.#seeds.push({ args: { ...args }, replies: [reply] });
  }

  /** The `fetch` to hand to `FriendsClient`. */
  readonly fetch: typeof fetch = (input, init) => {
    const url = new URL(String(input), "http://mock.invalid");
    const args: SendArgs = {
      method: String(init?.method ?? "GET"),
      path: url.pathname + url.search,
      body: init?.body === undefined || init?.body === null ? undefined : String(init.body),
    };
    this.#calls.push(args);
    const reply = this.#seeds.find((s) => sameArgs(s.args, args) && s.replies.length > 0)?.replies.shift();
    if (reply === undefined) {
      throw new Error(`mock_no_response_configured: send(${JSON.stringify(args)}) was called with no seeded response`);
    }
    if (reply instanceof Error) return Promise.reject(reply);
    const text = reply.body === undefined ? null : JSON.stringify(reply.body);
    return Promise.resolve(new Response(reply.status === 204 ? null : text, { status: reply.status }));
  };

  /** Throws when a seeded reply was never consumed, naming each call's arguments and how many are left. */
  assertAllSeedsConsumed(): void {
    const left = this.#seeds.filter((s) => s.replies.length > 0)
      .map((s) => `send(${JSON.stringify(s.args)}) has ${s.replies.length} left`);
    if (left.length > 0) throw new Error(`mock_unconsumed_responses: ${left.join("; ")}`);
  }
}

function sameArgs(a: SendArgs, b: SendArgs): boolean {
  return a.method === b.method && a.path === b.path && a.body === b.body;
}
