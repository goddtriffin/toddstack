import { assert, assertEquals, assertRejects, assertThrows } from "@std/assert";
import {
  Accounts,
  AccountsError,
  type JwkSet,
  type Profile,
  type RevocationPoller,
  Revocations,
  toddgriffin,
  toddgriffinAt,
  type WebhookEvent,
} from "./mod.ts";
import { base64url, utf8 } from "./encoding.ts";

const SUB = "4b0b2c6e-0000-4000-8000-000000000001";
const OTHER = "4b0b2c6e-0000-4000-8000-000000000002";
const BASIC = `Basic ${btoa("boggledygook:s3cret")}`;
const ISSUER = "http://127.0.0.1:8790/oidc";
const config = toddgriffinAt("http://127.0.0.1:8790", "boggledygook", "s3cret", "https://www.boggledygook.com/cb");

interface Seen {
  method: string;
  target: string;
  authorization: string | null;
  body: unknown;
}

type Reply = Response | [number, unknown];

/** A fake accounts API: records every request and answers request `n` with `answer(n, request)`. */
function fake(answer: (n: number, seen: Seen) => Reply): { fetch: typeof fetch; seen: Seen[] } {
  const seen: Seen[] = [];
  const fetcher: typeof fetch = async (input, init) => {
    const req = new Request(input, init);
    const url = new URL(req.url);
    const text = await req.text();
    const entry: Seen = {
      method: req.method,
      target: `${url.pathname}${url.search}`,
      authorization: req.headers.get("authorization"),
      body: text === "" ? null : JSON.parse(text),
    };
    seen.push(entry);
    const reply = answer(seen.length - 1, entry);
    if (reply instanceof Response) return reply;
    const [status, body] = reply;
    return status === 204 ? new Response(null, { status }) : Response.json(body, { status });
  };
  return { fetch: fetcher, seen };
}

const accountsFor = (f: { fetch: typeof fetch }, keys?: JwkSet) => new Accounts(config, { fetch: f.fetch, keys });

Deno.test("the profile reads every claim and the review and suspended flags", async () => {
  const f = fake(() => [200, {
    sub: SUB,
    username: "alex",
    preferred_username: "alex",
    name: "Alex",
    picture: "https://assets.toddgriffin.me/a.jpg",
    updated_at: 1_700_000_000,
    email: "a@b.c",
    email_verified: true,
    visibility: "review",
    suspended: true,
  }]);
  const profile: Profile = await accountsFor(f).player(SUB).profile();
  assertEquals(profile, {
    sub: SUB,
    username: "alex",
    preferred_username: "alex",
    name: "Alex",
    picture: "https://assets.toddgriffin.me/a.jpg",
    updated_at: 1_700_000_000,
    email: "a@b.c",
    email_verified: true,
    visibility: "review",
    suspended: true,
  });
  assertEquals(f.seen, [{ method: "GET", target: `/api/v1/players/${SUB}`, authorization: BASIC, body: null }]);
});

Deno.test("every route uses the documented method, path and body", async () => {
  const f = fake((_, seen) => {
    if (seen.target.endsWith("/friends/requests") && seen.method === "POST") {
      return [200, { result: "accepted", sub: OTHER }];
    }
    if (seen.target.endsWith("/events")) return [202, { delivered: 2, dropped: 0 }];
    if (seen.method === "GET") {
      return [200, {
        friends: [],
        relationships: {},
        incoming: [],
        outgoing: [],
        blocked: [],
        hidden: [],
        items: [],
        mutes: [],
      }];
    }
    return [204, null];
  });
  const p = accountsFor(f).player(SUB);
  await p.friends();
  await p.relationships([OTHER, "u3"]);
  await p.requests();
  assertEquals(await p.request(OTHER), "accepted");
  await p.requestByUsername("alex");
  await p.requestByCode("ABCD-1234");
  await p.accept(OTHER);
  await p.decline(OTHER);
  await p.cancel(OTHER);
  await p.remove(OTHER);
  await p.blocks();
  await p.block(OTHER);
  await p.unblock(OTHER);
  await p.postEvent({ kind: "beat_score", title: "Alex won" }, "game-1");
  await p.inbox(20);
  await p.markRead("i1");
  await p.mutes();
  await p.mute("beat_score");
  await p.mute("*");
  await p.unmute("beat_score");
  await p.setTimeZone("America/Chicago");
  await p.report(OTHER, "cheating", "bot");

  const base = `/api/v1/players/${SUB}`;
  assertEquals(f.seen.map((s) => [s.method, s.target, s.body]), [
    ["GET", `${base}/friends`, null],
    ["GET", `${base}/friends/relationships?subs=${OTHER}%2Cu3`, null],
    ["GET", `${base}/friends/requests`, null],
    ["POST", `${base}/friends/requests`, { sub: OTHER }],
    ["POST", `${base}/friends/requests`, { username: "alex" }],
    ["POST", `${base}/friends/requests`, { friend_code: "ABCD-1234" }],
    ["POST", `${base}/friends/requests/${OTHER}/accept`, null],
    ["POST", `${base}/friends/requests/${OTHER}/decline`, null],
    ["DELETE", `${base}/friends/requests/${OTHER}`, null],
    ["DELETE", `${base}/friends/${OTHER}`, null],
    ["GET", `${base}/blocks`, null],
    ["PUT", `${base}/blocks/${OTHER}`, null],
    ["DELETE", `${base}/blocks/${OTHER}`, null],
    ["POST", `${base}/events`, { kind: "beat_score", title: "Alex won", body: "", idempotency_key: "game-1" }],
    ["GET", `${base}/inbox?limit=20`, null],
    ["POST", `${base}/inbox/i1/read`, null],
    ["GET", `${base}/mutes`, null],
    ["PUT", `${base}/mutes/beat_score`, null],
    ["PUT", `${base}/mutes/*`, null],
    ["DELETE", `${base}/mutes/beat_score`, null],
    ["PUT", `${base}/time-zone`, { time_zone: "America/Chicago" }],
    ["POST", "/api/v1/reports", { reporter_sub: SUB, sub: OTHER, reason: "cheating", note: "bot" }],
  ]);
  for (const s of f.seen) assertEquals(s.authorization, BASIC, s.target);
});

Deno.test("an error carries the status, code and message and is not retried", async () => {
  const f = fake(() => [404, { error: "not_found", message: "Not found." }]);
  const e = await assertRejects(() => accountsFor(f).player(SUB).request(OTHER), AccountsError);
  assertEquals([e.kind, e.status, e.code], ["server", 404, "not_found"]);
  assert(e.message.includes("Not found."));
  assertEquals(f.seen.length, 1);
});

Deno.test("a suspended players write is final with its code, and a rate limit is readable", async () => {
  const suspended = fake(() => [403, { error: "suspended", message: "Suspended." }]);
  const a = await assertRejects(() => accountsFor(suspended).player(SUB).block(OTHER), AccountsError);
  assertEquals([a.code, suspended.seen.length], ["suspended", 1]);
  const limited = fake(() => [429, { error: "rate_limited", message: "Slow down." }]);
  const b = await assertRejects(() => accountsFor(limited).player(SUB).request(OTHER), AccountsError);
  assertEquals(b.code, "rate_limited");
});

Deno.test("a write is retried after a server error because every write is idempotent", async () => {
  const f = fake((n) => n === 0 ? [503, { error: "server_error", message: "x" }] : [204, null]);
  await accountsFor(f).player(SUB).block(OTHER);
  assertEquals(f.seen.map((s) => s.method), ["PUT", "PUT"]);
});

Deno.test("an event needs a key of one to a hundred characters and sends nothing without", async () => {
  const f = fake(() => [202, { delivered: 0 }]);
  const p = accountsFor(f).player(SUB);
  for (const key of ["", "k".repeat(101)]) {
    const e = await assertRejects(() => p.postEvent({ kind: "k", title: "t" }, key), AccountsError);
    assertEquals(e.code, "bad_request");
  }
  assertEquals(f.seen.length, 0);
  await p.postEvent({ kind: "k", title: "t" }, "k".repeat(100));
  assertEquals(f.seen.length, 1);
});

Deno.test("a replayed event says so and carries the counts", async () => {
  const f = fake(() => [202, {
    delivered: 3,
    dropped: 1,
    replayed: true,
    counts: { delivered: 3, scheduled: 0, not_delivered: 2, dropped_runaway: 1 },
  }]);
  assertEquals(await accountsFor(f).player(SUB).postEvent({ kind: "k", title: "t" }, "k"), {
    replayed: true,
    delivered: 3,
    dropped: 1,
    counts: { delivered: 3, scheduled: 0, notDelivered: 2, droppedRunaway: 1 },
  });
});

Deno.test("relationships over a hundred subs go out as several calls", async () => {
  const f = fake(() => [200, { relationships: { a: "friends" } }]);
  const subs = Array.from({ length: 150 }, (_, n) => `u${n}`);
  const found = await accountsFor(f).player(SUB).relationships(subs);
  assertEquals([f.seen.length, found.a], [2, "friends"]);
});

Deno.test("a client without a secret or a path-unsafe id is refused before any request", async () => {
  const f = fake(() => [204, null]);
  const open = new Accounts({ ...config, clientSecret: null }, { fetch: f.fetch });
  assertEquals(assertThrows(() => open.player(SUB), AccountsError).kind, "config");
  const accounts = accountsFor(f);
  assertEquals(assertThrows(() => accounts.player("../x"), AccountsError).code, "bad_request");
  const p = accounts.player(SUB);
  for (const bad of ["a/b", "", "a?b", "..", "a%2Fb"]) {
    assertEquals((await assertRejects(() => p.accept(bad), AccountsError)).code, "bad_request", bad);
    assertEquals((await assertRejects(() => p.mute(bad), AccountsError)).code, "bad_request", bad);
  }
  const e = await assertRejects(
    () => accounts.reportAsPlayer({ reporterSub: "a/b", sub: OTHER, reason: "name" }),
    AccountsError,
  );
  assertEquals(e.kind, "server");
  assertEquals(f.seen.length, 0);
});

// ---- local verification and instant logout

const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, [
  "sign",
  "verify",
]) as CryptoKeyPair;
const jwks: JwkSet = {
  keys: [{ ...(await crypto.subtle.exportKey("jwk", pair.publicKey)), kid: "k1", alg: "ES256", use: "sig" }],
};
const now = () => Math.floor(Date.now() / 1000);

async function accessToken(claims: Record<string, unknown> = {}): Promise<string> {
  const h = base64url(utf8(JSON.stringify({ alg: "ES256", kid: "k1", typ: "at+jwt" })));
  const c = base64url(utf8(JSON.stringify({
    iss: ISSUER,
    aud: "boggledygook",
    sub: SUB,
    client_id: "boggledygook",
    scope: "openid",
    exp: now() + 600,
    ...claims,
  })));
  const sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, utf8(`${h}.${c}`));
  return `${h}.${c}.${base64url(new Uint8Array(sig))}`;
}

async function signed(secret: string, t: number, body: string): Promise<string> {
  const key = await crypto.subtle.importKey("raw", utf8(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, utf8(`${t}.${body}`)));
  return `t=${t},v1=${Array.from(mac, (b) => b.toString(16).padStart(2, "0")).join("")}`;
}

const iso = (unix: number) => new Date(unix * 1000).toISOString();

Deno.test("a family is revoked until its expiry and repeats keep the later expiry", () => {
  const set = new Revocations();
  assert(!set.isRevoked("f1"));
  set.revoke("f1", now() + 60);
  set.revoke("f1", now() + 600);
  set.revoke("f1", now() + 10);
  set.revoke("old", now() - 1);
  assert(set.isRevoked("f1"));
  assert(!set.isRevoked("old"));
  assert(!set.isRevoked("other"));
  assertEquals(set.size, 1);
});

Deno.test("a verified tokens.revoked webhook kills those families at once", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  const dead = await accessToken({ fam: "f-dead" });
  const live = await accessToken({ fam: "f-live" });
  assertEquals((await accounts.verifyAccessToken(dead)).fam, "f-dead");

  const body = JSON.stringify({
    id: "e1",
    type: "tokens.revoked",
    created_at: now(),
    data: { sub: SUB, families: ["f-dead", "f-dead", "f-x"] },
  });
  const event = await accounts.handleWebhook("whsec", await signed("whsec", now(), body), body);
  assertEquals(event.data.families, ["f-dead", "f-dead", "f-x"]);

  const refused = await assertRejects(() => accounts.verifyAccessToken(dead), AccountsError);
  assertEquals([refused.kind, refused.message], ["invalid_token", "token rejected: token revoked"]);
  assert((await accounts.verifyAccessToken(live)).fam === "f-live");
});

Deno.test("a webhook with a bad signature or another type revokes nothing", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  const body = JSON.stringify({
    id: "e1",
    type: "tokens.revoked",
    created_at: now(),
    data: { sub: SUB, families: ["f1"] },
  });
  await assertRejects(
    async () => accounts.handleWebhook("whsec", await signed("wrong", now(), body), body),
    AccountsError,
  );
  const other = JSON.stringify({
    id: "e2",
    type: "user.deleted",
    created_at: now(),
    data: { sub: SUB, families: ["f1"] },
  });
  await accounts.handleWebhook("whsec", await signed("whsec", now(), other), other);
  assertEquals(accounts.revocations.size, 0);
  const event: WebhookEvent = { id: "x", type: "user.deleted", createdAt: 0, data: { families: ["f"] } };
  assertEquals(accounts.revocations.apply(event), 0);
});

Deno.test("a token without a family still verifies and the other checks still run", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  assertEquals((await accounts.verifyAccessToken(await accessToken())).fam, undefined);
  await accounts.verifyAccessToken(await accessToken({ exp: now() - 10 })); // small skew allowed
  await assertRejects(async () => accounts.verifyAccessToken(await accessToken({ exp: now() - 120 })), AccountsError);
  await assertRejects(
    async () => accounts.verifyAccessToken(await accessToken({ iss: "https://evil.example/oidc" })),
    AccountsError,
  );
});

Deno.test("a poll records families, dedupes repeats and moves the cursor", async () => {
  const t1 = now() + 600;
  const f = fake((n) =>
    n === 0
      ? [200, {
        families: [{ fam: "f1", expires_at: iso(t1) }, { fam: "f1", expires_at: iso(t1) }],
        now: "2026-11-18T10:00:00Z",
      }]
      : [200, { families: [{ fam: "f2", expires_at: iso(t1) }], now: "2026-11-18T10:00:30Z" }]
  );
  const accounts = accountsFor(f);
  assertEquals(await accounts.pollRevocations(null), { next: "2026-11-18T10:00:00Z", listed: 2 });
  assertEquals(await accounts.pollRevocations("2026-11-18T10:00:00Z"), { next: "2026-11-18T10:00:30Z", listed: 1 });
  assertEquals(accounts.revocations.size, 2);
  assert(accounts.revocations.isRevoked("f1") && accounts.revocations.isRevoked("f2"));
  assertEquals(f.seen.map((s) => [s.method, s.target, s.authorization]), [
    ["GET", "/api/v1/revoked-tokens", BASIC],
    ["GET", "/api/v1/revoked-tokens?since=2026-11-18T10%3A00%3A00Z", BASIC],
  ]);
});

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

Deno.test("the poller polls at startup before resolving, keeps polling, and stops", async () => {
  const f = fake((n) => [200, { families: [{ fam: `f${n}`, expires_at: iso(now() + 600) }], now: iso(now()) }]);
  const accounts = accountsFor(f);
  const poller: RevocationPoller = await accounts.startRevocationPoller({ intervalMs: 30 });
  assert(accounts.revocations.isRevoked("f0")); // the startup poll is awaited
  await sleep(250);
  assert(accounts.revocations.isRevoked("f2"), `${f.seen.length} polls`);
  poller.stop();
  await sleep(60);
  const before = f.seen.length;
  await sleep(150);
  assertEquals(f.seen.length, before);
});

Deno.test("a failing poll reaches onError, keeps verifying and recovers", async () => {
  const f = fake((n) =>
    n < 3
      ? [500, { error: "server_error", message: "x" }]
      : [200, { families: [{ fam: "late", expires_at: iso(now() + 600) }], now: iso(now()) }]
  );
  const accounts = new Accounts(config, { keys: jwks, fetch: f.fetch });
  const errors: unknown[] = [];
  const poller = await accounts.startRevocationPoller({ intervalMs: 30, onError: (e) => errors.push(e) });
  assertEquals(errors.length, 1);
  await accounts.verifyAccessToken(await accessToken({ fam: "late" }));
  await sleep(2_000);
  assert(accounts.revocations.isRevoked("late"));
  poller.stop();
});

Deno.test("withRevocationPoller wires the client and the poller; the poller needs the client secret", async () => {
  const f = fake(() => [200, { families: [{ fam: "f0", expires_at: iso(now() + 600) }], now: iso(now()) }]);
  const { accounts, poller } = await Accounts.withRevocationPoller(config, { fetch: f.fetch, intervalMs: 1_000 });
  assert(accounts.revocations.isRevoked("f0"));
  poller.stop();
  const open = new Accounts({ ...toddgriffin("boggledygook", null, "x") });
  assertEquals((await assertRejects(() => open.startRevocationPoller(), AccountsError)).kind, "config");
});
