import { assertEquals, assertRejects } from "@std/assert";
import { FriendsClient, FriendsClientError } from "./mod.ts";

type Call = { url: string; method: string; body: string | undefined; headers: Headers };

function client(status: number, answer: unknown, calls: Call[] = []): FriendsClient {
  return new FriendsClient({
    headers: () => ({ authorization: "Bearer app" }),
    fetch: (input, init) => {
      calls.push({
        url: String(input),
        method: String(init?.method),
        body: init?.body as string | undefined,
        headers: new Headers(init?.headers),
      });
      return Promise.resolve(
        new Response(answer === undefined ? null : JSON.stringify(answer), { status }),
      );
    },
  });
}

const alex = { sub: "u2", username: "alex", name: null, picture: null, since: "2026-11-15T10:00:00Z" };

Deno.test("list and requests read the server's JSON, with the caller's headers", async () => {
  const calls: Call[] = [];
  assertEquals([alex], await client(200, { friends: [alex] }, calls).list());
  assertEquals({ incoming: [alex], outgoing: [] }, await client(200, { incoming: [alex], outgoing: [] }).requests());
  assertEquals(["GET", "/api/v1/friends", "Bearer app"], [
    calls[0].method,
    calls[0].url,
    calls[0].headers.get("authorization"),
  ]);
});

Deno.test("sending by username and by code posts the matching body and returns the result", async () => {
  const calls: Call[] = [];
  const friends = client(200, { result: "accepted" }, calls);
  assertEquals("accepted", await friends.requestByUsername("alex"));
  assertEquals("accepted", await friends.requestByCode("ABCD2345"));
  assertEquals(
    [
      ["POST", "/api/v1/friends/requests", '{"username":"alex"}', "application/json"],
      ["POST", "/api/v1/friends/requests", '{"friend_code":"ABCD2345"}', "application/json"],
    ],
    calls.map((c) => [c.method, c.url, c.body, c.headers.get("content-type")]),
  );
});

Deno.test("accept, decline, cancel, remove, block and unblock hit their routes", async () => {
  const calls: Call[] = [];
  const friends = client(204, undefined, calls);
  await friends.accept("u2");
  await friends.decline("u3");
  await friends.cancel("u4");
  await friends.remove("u5");
  await friends.block("u6");
  await friends.unblock("u7");
  assertEquals(
    [
      "POST /api/v1/friends/requests/u2/accept",
      "POST /api/v1/friends/requests/u3/decline",
      "DELETE /api/v1/friends/requests/u4",
      "DELETE /api/v1/friends/u5",
      "POST /api/v1/friends/blocks",
      "DELETE /api/v1/friends/blocks/u7",
    ],
    calls.map((c) => `${c.method} ${c.url}`),
  );
  assertEquals('{"sub":"u6"}', calls[4].body);
});

Deno.test("blocks and relationships read through", async () => {
  const calls: Call[] = [];
  assertEquals(
    { blocked: [alex], hidden: ["u2"] },
    await client(200, { blocked: [alex], hidden: ["u2"] }).blocks(),
  );
  assertEquals(
    { u2: "friends", u3: "none" },
    await client(200, { relationships: { u2: "friends", u3: "none" } }, calls).relationships(["u2", "u3"]),
  );
  assertEquals("/api/v1/friends/relationships?subs=u2%2Cu3", calls[0].url);
});

Deno.test("a server error becomes a FriendsClientError with its code and the person's message", async () => {
  const error = await assertRejects(
    () => client(429, { error: "rate_limited", message: "Please wait an hour." }).requestByUsername("x"),
    FriendsClientError,
  );
  assertEquals([429, "rate_limited", "Please wait an hour."], [error.status, error.code, error.message]);
  const guest = await assertRejects(() => client(401, { error: "account_required", message: "Sign in." }).list());
  assertEquals("account_required", (guest as FriendsClientError).code);
});

Deno.test("a non-JSON failure and a network failure are still FriendsClientErrors", async () => {
  const proxy = new FriendsClient({ fetch: () => Promise.resolve(new Response("<html>", { status: 502 })) });
  const a = await assertRejects(() => proxy.list(), FriendsClientError);
  assertEquals([502, "accounts_unavailable"], [a.status, a.code]);
  const offline = new FriendsClient({ fetch: () => Promise.reject(new TypeError("offline")) });
  const b = await assertRejects(() => offline.list(), FriendsClientError);
  assertEquals([0, "network", "offline"], [b.status, b.code, b.message]);
});
