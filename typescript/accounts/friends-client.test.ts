import { assertEquals, assertRejects, assertThrows } from "@std/assert";
import { FriendsClient, FriendsClientError, MockFriendsFetch } from "./mod.ts";

const F = "/api/v1/friends";
const alex = { sub: "u2", username: "alex", name: null, picture: null, since: "2026-11-15T10:00:00Z" };

function setup(options: { signedIn?: () => boolean } = {}): [FriendsClient, MockFriendsFetch] {
  const mock = new MockFriendsFetch();
  return [new FriendsClient({ ...options, fetch: mock.fetch, headers: () => ({ authorization: "Bearer app" }) }), mock];
}

Deno.test("list and requests read the server's JSON", async () => {
  const [friends, mock] = setup();
  mock.seed({ method: "GET", path: F }, { status: 200, body: { friends: [alex] } });
  mock.seed({ method: "GET", path: `${F}/requests` }, { status: 200, body: { incoming: [alex], outgoing: [] } });
  assertEquals([alex], await friends.list());
  assertEquals({ incoming: [alex], outgoing: [] }, await friends.requests());
  mock.assertAllSeedsConsumed();
});

Deno.test("sending by username and by code posts the matching body and returns the result", async () => {
  const [friends, mock] = setup();
  mock.seed({ method: "POST", path: `${F}/requests`, body: '{"username":"alex"}' }, {
    status: 200,
    body: { result: "accepted" },
  });
  mock.seed({ method: "POST", path: `${F}/requests`, body: '{"friend_code":"ABCD2345"}' }, {
    status: 200,
    body: { result: "requested" },
  });
  assertEquals("accepted", await friends.requestByUsername("alex"));
  assertEquals("requested", await friends.requestByCode("ABCD2345"));
  mock.assertAllSeedsConsumed();
});

Deno.test("accept, decline, cancel, remove, block and unblock hit their routes", async () => {
  const [friends, mock] = setup();
  const ok = { status: 204 };
  mock.seed({ method: "POST", path: `${F}/requests/u2/accept` }, ok);
  mock.seed({ method: "POST", path: `${F}/requests/u3/decline` }, ok);
  mock.seed({ method: "DELETE", path: `${F}/requests/u4` }, ok);
  mock.seed({ method: "DELETE", path: `${F}/u5` }, ok);
  mock.seed({ method: "POST", path: `${F}/blocks`, body: '{"sub":"u6"}' }, ok);
  mock.seed({ method: "DELETE", path: `${F}/blocks/u7` }, ok);
  await friends.accept("u2");
  await friends.decline("u3");
  await friends.cancel("u4");
  await friends.remove("u5");
  await friends.block("u6");
  await friends.unblock("u7");
  mock.assertAllSeedsConsumed();
});

Deno.test("blocks and relationships read through", async () => {
  const [friends, mock] = setup();
  mock.seed({ method: "GET", path: `${F}/blocks` }, { status: 200, body: { blocked: [alex], hidden: ["u2"] } });
  mock.seed({ method: "GET", path: `${F}/relationships?subs=u2%2Cu3` }, {
    status: 200,
    body: { relationships: { u2: "friends", u3: "none" } },
  });
  assertEquals({ blocked: [alex], hidden: ["u2"] }, await friends.blocks());
  assertEquals({ u2: "friends", u3: "none" }, await friends.relationships(["u2", "u3"]));
  mock.assertAllSeedsConsumed();
});

Deno.test("a server error becomes a FriendsClientError with its code and the person's message", async () => {
  const [friends, mock] = setup();
  mock.seed({ method: "POST", path: `${F}/requests`, body: '{"username":"x"}' }, {
    status: 429,
    body: { error: "rate_limited", message: "Please wait an hour." },
  });
  const error = await assertRejects(() => friends.requestByUsername("x"), FriendsClientError);
  assertEquals([429, "rate_limited", "Please wait an hour."], [error.status, error.code, error.message]);
  mock.assertAllSeedsConsumed();
});

Deno.test("a non-JSON failure and a network failure are still FriendsClientErrors", async () => {
  const proxy = new FriendsClient({ fetch: () => Promise.resolve(new Response("<html>", { status: 502 })) });
  const a = await assertRejects(() => proxy.list(), FriendsClientError);
  assertEquals([502, "accounts_unavailable"], [a.status, a.code]);
  const [friends, mock] = setup();
  mock.seed({ method: "GET", path: F }, new TypeError("offline"));
  const b = await assertRejects(() => friends.list(), FriendsClientError);
  assertEquals([0, "network", "offline"], [b.status, b.code, b.message]);
});

Deno.test("with no session, nothing is sent and the error is account_required", async () => {
  const [friends, mock] = setup({ signedIn: () => false });
  const error = await assertRejects(() => friends.list(), FriendsClientError);
  assertEquals([401, "account_required"], [error.status, error.code]);
  assertEquals([], mock.calls);
});

Deno.test("an id that could leave the friends paths is refused before any request", async () => {
  const [friends, mock] = setup();
  for (const bad of ["../../oidc/x", "..", "a/b", "a?b=1", "a#b", "a%2Fb", ""]) {
    for (
      const call of [friends.accept, friends.decline, friends.cancel, friends.remove, friends.block, friends.unblock]
    ) {
      const error = await assertRejects(() => call.call(friends, bad), FriendsClientError);
      assertEquals([400, "bad_request"], [error.status, error.code], bad);
    }
    await assertRejects(() => friends.relationships(["u2", bad]), FriendsClientError);
  }
  assertEquals([], mock.calls);
});

Deno.test("an unseeded call throws and a leftover seed fails the check", () => {
  const mock = new MockFriendsFetch();
  assertThrows(() => mock.fetch("/api/v1/friends"), Error, "mock_no_response_configured");
  mock.seed({ method: "GET", path: F }, { status: 200 });
  assertThrows(() => mock.assertAllSeedsConsumed(), Error, "mock_unconsumed_responses");
});
