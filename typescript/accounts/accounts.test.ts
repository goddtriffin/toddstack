import { assert, assertEquals, assertNotEquals, assertRejects } from "@std/assert";
import {
  Accounts,
  AccountsError,
  accountUrl,
  endSessionUrl,
  hasScope,
  isKnownWebhookEventType,
  type JwkSet,
  parseCallback,
  pkceChallenge,
  postLogoutTarget,
  safeReturnTo,
  startLogin,
  toddgriffin,
  verifyWebhook,
  VERSION,
  WEBHOOK_EVENT_TYPES,
} from "./mod.ts";
import { base64url, utf8 } from "./encoding.ts";
import { Keys } from "./jwks.ts";

const config = toddgriffin("boggledygook", "s3cret", "https://www.boggledygook.com/auth/callback");

Deno.test("startLogin builds a PKCE request", async () => {
  const { url, pending } = await startLogin(config, { scopes: ["profile"], returnTo: "/play" });
  const parsed = new URL(url);
  assertEquals(`${parsed.origin}${parsed.pathname}`, "https://accounts.toddgriffin.me/oidc/auth");
  const q = parsed.searchParams;
  assertEquals(q.get("scope"), "openid profile");
  assertEquals(q.get("code_challenge"), await pkceChallenge(pending.verifier));
  assertEquals(q.get("code_challenge_method"), "S256");
  assertEquals(q.get("state"), pending.state);
  assertEquals(q.get("nonce"), pending.nonce);
  assertEquals(pending.returnTo, "/play");
  assertNotEquals(pending.state, pending.nonce);
});

Deno.test("startLogin options", async () => {
  const { url, pending } = await startLogin(config, {
    prompt: "none",
    loginHint: "a@b.c",
    maxAge: 60,
    returnTo: "https://evil.example",
  });
  const q = new URL(url).searchParams;
  assertEquals([q.get("prompt"), q.get("login_hint"), q.get("max_age")], ["none", "a@b.c", "60"]);
  assertEquals(pending.returnTo, null);
});

Deno.test("challenge is base64url sha256", async () => {
  assertEquals(await pkceChallenge("abc"), "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
});

Deno.test("callbacks parse", () => {
  const c = parseCallback("?code=c1&state=s1&iss=https%3A%2F%2Fa");
  assertEquals([c.code, c.iss], ["c1", "https://a"]);
  assertEquals(parseCallback("error=access_denied&state=s1").error, "access_denied");
});

Deno.test("safeReturnTo keeps only local paths", () => {
  assertEquals(safeReturnTo("/play?level=2"), "/play?level=2");
  for (
    const bad of [
      "//evil.example",
      "https://evil.example",
      "/\\evil.example",
      "play",
      "/a\r\nx",
      "",
      "/" + "a".repeat(1000),
    ]
  ) {
    assertEquals(safeReturnTo(bad), null, JSON.stringify(bad));
  }
});

async function sign(secret: string, t: number, body: string): Promise<string> {
  const key = await crypto.subtle.importKey("raw", utf8(secret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, utf8(`${t}.${body}`)));
  return `t=${t},v1=${Array.from(mac, (b) => b.toString(16).padStart(2, "0")).join("")}`;
}

const BODY = '{"id":"e1","type":"user.deleted","created_at":1700000000,"data":{"sub":"u1"}}';

Deno.test("valid webhooks verify", async () => {
  const event = await verifyWebhook("secret", await sign("secret", 1_700_000_000, BODY), BODY, 1_700_000_010);
  assertEquals([event.type, event.data.sub, event.createdAt], ["user.deleted", "u1", 1_700_000_000]);
});

Deno.test("tampering, wrong secret and age are refused", async () => {
  const header = await sign("secret", 1_700_000_000, BODY);
  const bad = [
    () => verifyWebhook("secret", header, BODY.replace("u1", "u2"), 1_700_000_000),
    () => verifyWebhook("other", header, BODY, 1_700_000_000),
    () => verifyWebhook("secret", header, BODY, 1_700_000_301),
    () => verifyWebhook("secret", "nonsense", BODY, 1_700_000_000),
    () => verifyWebhook("secret", null, BODY, 1_700_000_000),
  ];
  for (const p of bad) {
    const e = await assertRejects(p, AccountsError);
    assertEquals(e.kind, "invalid_webhook");
  }
});

Deno.test("friendship events name both people", async () => {
  for (const type of ["friendship.created", "friendship.removed"]) {
    const body = `{"id":"e2","type":"${type}","created_at":1700000000,"data":{"sub":"u1","friend_sub":"u2"}}`;
    const event = await verifyWebhook("secret", await sign("secret", 1_700_000_000, body), body, 1_700_000_000);
    assertEquals(event, { id: "e2", type, createdAt: 1_700_000_000, data: { sub: "u1", friend_sub: "u2" } });
    assert(isKnownWebhookEventType(event.type));
  }
});

Deno.test("friend request events name the sender and the receiver", async () => {
  for (const type of ["friend_request.created", "friend_request.cancelled", "friend_request.declined"]) {
    const body =
      `{"id":"e4","type":"${type}","created_at":1700000000,"data":{"sub":"u2","from_sub":"u1","to_sub":"u2"}}`;
    const event = await verifyWebhook("secret", await sign("secret", 1_700_000_000, body), body, 1_700_000_000);
    assertEquals(event, {
      id: "e4",
      type,
      createdAt: 1_700_000_000,
      data: { sub: "u2", from_sub: "u1", to_sub: "u2" },
    });
    assert(isKnownWebhookEventType(event.type));
  }
});

Deno.test("every event type the service sends is known, and newer ones still verify", async () => {
  assertEquals([...WEBHOOK_EVENT_TYPES], [
    "user.updated",
    "user.deleted",
    "user.suspended",
    "user.reactivated",
    "user.signed_out",
    "grant.revoked",
    "friendship.created",
    "friendship.removed",
    "friend_request.created",
    "friend_request.cancelled",
    "friend_request.declined",
  ]);
  const body = '{"id":"e3","type":"something.new","created_at":1700000000,"data":{"sub":"u1"}}';
  const event = await verifyWebhook("secret", await sign("secret", 1_700_000_000, body), body, 1_700_000_000);
  assertEquals(event.type, "something.new");
  assert(!isKnownWebhookEventType(event.type));
});

// A signing key, its JWKS, and a fake accounts service.
const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, [
  "sign",
  "verify",
]) as CryptoKeyPair;
const jwks: JwkSet = {
  keys: [{ ...(await crypto.subtle.exportKey("jwk", pair.publicKey)), kid: "k1", alg: "ES256", use: "sig" }],
};

async function jwt(claims: Record<string, unknown>, header: Record<string, unknown> = {}): Promise<string> {
  const h = base64url(utf8(JSON.stringify({ alg: "ES256", kid: "k1", ...header })));
  const c = base64url(utf8(JSON.stringify(claims)));
  const sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, utf8(`${h}.${c}`));
  return `${h}.${c}.${base64url(new Uint8Array(sig))}`;
}

const now = () => Math.floor(Date.now() / 1000);
const idClaims = (nonce: string) => ({
  iss: config.issuer,
  aud: "boggledygook",
  sub: "u1",
  exp: now() + 600,
  nonce,
  amr: ["pwd"],
});

Deno.test("finishLogin exchanges the code and verifies the ID token", async () => {
  const seen: Request[] = [];
  let nonce = "";
  const fake = async (input: string | URL | Request, init?: RequestInit) => {
    const req = new Request(input, init);
    seen.push(req.clone());
    if (req.url.endsWith("/oidc/jwks")) return Response.json(jwks);
    if (req.url.endsWith("/oidc/token")) {
      return Response.json({
        access_token: "at",
        id_token: await jwt(idClaims(nonce)),
        refresh_token: "rt",
        expires_in: 600,
        scope: "openid",
      });
    }
    return new Response("no", { status: 404 });
  };
  const accounts = new Accounts(config, { fetch: fake });
  const { pending } = await accounts.startLogin({ returnTo: "/play" });
  nonce = pending.nonce;
  const signedIn = await accounts.finishLogin(
    pending,
    `?code=c1&state=${pending.state}&iss=${encodeURIComponent(config.issuer)}`,
  );
  assertEquals([signedIn.claims.sub, signedIn.tokens.refreshToken, signedIn.returnTo], ["u1", "rt", "/play"]);
  const token = seen.find((r) => r.url.endsWith("/token"))!;
  assertEquals(token.headers.get("authorization"), `Basic ${btoa("boggledygook:s3cret")}`);
  const form = new URLSearchParams(await token.text());
  assertEquals([form.get("code"), form.get("code_verifier")], ["c1", pending.verifier]);
  assertEquals(token.headers.get("user-agent"), `toddstack-accounts/${VERSION}`);
});

Deno.test("finishLogin refuses forged and cancelled callbacks", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  const { pending } = await accounts.startLogin();
  const kinds = async (q: string) => (await assertRejects(() => accounts.finishLogin(pending, q), AccountsError)).kind;
  assertEquals(await kinds("code=c&state=wrong"), "state_mismatch");
  assertEquals(await kinds(`code=c&state=${pending.state}&iss=https://evil`), "state_mismatch");
  assertEquals(await kinds(`error=access_denied&state=${pending.state}`), "authorization");
  assertEquals(await kinds(`state=${pending.state}`), "missing_code");
});

Deno.test("token checks", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  assertEquals((await accounts.verifyIdToken(await jwt(idClaims("n")), "n")).sub, "u1");
  const t = await jwt(idClaims("n"));
  const bad = [
    () => accounts.verifyIdToken(t, "other"),
    async () => accounts.verifyIdToken(await jwt({ ...idClaims("n"), aud: "someone-else" })),
    async () => accounts.verifyIdToken(await jwt({ ...idClaims("n"), iss: "https://evil" })),
    async () => accounts.verifyIdToken(await jwt({ ...idClaims("n"), exp: now() - 60 })),
    async () => accounts.verifyIdToken(await jwt(idClaims("n"), { kid: "unknown" })),
    async () => accounts.verifyIdToken(await jwt(idClaims("n"), { alg: "HS256" })),
    () => accounts.verifyIdToken(t.slice(0, -4) + "AAAA"),
    () => accounts.verifyAccessToken(t), // no typ at+jwt
  ];
  for (const p of bad) assertEquals((await assertRejects(p, AccountsError)).kind, "invalid_token");
  const at = await accounts.verifyAccessToken(
    await jwt({
      iss: config.issuer,
      aud: "boggledygook",
      sub: "u1",
      client_id: "boggledygook",
      scope: "openid friends",
      exp: now() + 60,
    }, { typ: "at+jwt" }),
  );
  assert(hasScope(at, "friends") && !hasScope(at, "friend"));
});

Deno.test("refresh failures are token_refused; public clients send client_id", async () => {
  let body = "";
  const accounts = new Accounts({ ...config, clientSecret: null }, {
    keys: jwks,
    fetch: async (input, init) => {
      body = await new Request(input, init).text();
      return Response.json({ error: "invalid_grant" }, { status: 400 });
    },
  });
  const e = await assertRejects(() => accounts.refresh("rt"), AccountsError);
  assertEquals([e.kind, e.error], ["token_refused", "invalid_grant"]);
  assertEquals(new URLSearchParams(body).get("client_id"), "boggledygook");
});

Deno.test("friends API calls the service and maps fields", async () => {
  const calls: string[] = [];
  const accounts = new Accounts(config, {
    keys: jwks,
    fetch: async (input, init) => {
      const req = new Request(input, init);
      calls.push(
        `${req.method} ${new URL(req.url).pathname}${new URL(req.url).search} ${
          req.headers.get("authorization")
        } ${await req.text()}`,
      );
      if (req.url.endsWith("/events")) return Response.json({ delivered: 2 });
      if (req.url.includes("/inbox?")) {
        return Response.json({
          items: [{
            id: "i1",
            from: "u2",
            from_username: "alex",
            kind: "k",
            title: "t",
            body: "",
            url: null,
            created_at: "c",
            read_at: null,
          }],
        });
      }
      if (req.url.endsWith("/friends/requests") && req.method === "POST") {
        return Response.json({ result: "already_friends" });
      }
      if (req.url.endsWith("/blocks/u2")) return Response.json({ error: "not found" }, { status: 404 });
      return new Response(null, { status: 204 });
    },
  });
  const friends = accounts.friends("tok");
  assertEquals(
    await friends.postEvent({ kind: "daily_completed", title: "Alex finished the daily", collapseKey: "d1" }),
    2,
  );
  assertEquals((await friends.inbox(20))[0].fromUsername, "alex");
  assertEquals(await friends.requestByUsername("alex"), "already_friends");
  await friends.mute("*");
  const e = await assertRejects(() => friends.unblock("u2"), AccountsError);
  assertEquals([e.kind, e.status, e.message], ["api", 404, "accounts API returned 404: not found"]);
  assertEquals(
    calls[0],
    'POST /api/v1/events Bearer tok {"kind":"daily_completed","title":"Alex finished the daily","body":"","collapse_key":"d1"}',
  );
  assertEquals(calls[1].split(" ").slice(0, 2).join(" "), "GET /api/v1/inbox?limit=20");
  assertEquals(calls[3].split(" ").slice(0, 2).join(" "), "PUT /api/v1/mutes/*");
});

Deno.test("relationships ask in chunks of 100 and answer for each person", async () => {
  const asked: string[][] = [];
  const accounts = new Accounts(config, {
    keys: jwks,
    fetch: (input, init) => {
      const url = new URL(new Request(input, init).url);
      assertEquals(url.pathname, "/api/v1/friends/relationships");
      const subs = url.searchParams.get("subs")!.split(",");
      asked.push(subs);
      return Promise.resolve(
        Response.json({
          relationships: Object.fromEntries(subs.map((s) => [s, s === "u0" ? "friends" : "none"])),
        }),
      );
    },
  });
  const friends = accounts.friends("tok");
  const subs = Array.from({ length: 150 }, (_, i) => `u${i}`);
  const found = await friends.relationships(subs);
  assertEquals(asked.map((a) => a.length), [100, 50]);
  assertEquals(Object.keys(found).length, 150);
  assertEquals([found.u0, found.u149], ["friends", "none"]);
  assertEquals(await friends.relationship("u0"), "friends");
  assertEquals(await friends.relationships([]), {});
});

Deno.test("versions agree", async () => {
  const deno = JSON.parse((await Deno.readTextFile(new URL("./deno.jsonc", import.meta.url))).replace(/\/\/.*$/gm, ""));
  const npm = JSON.parse(await Deno.readTextFile(new URL("./package.json", import.meta.url)));
  assertEquals([deno.version, npm.version], [VERSION, VERSION]);
});

Deno.test("the end-session URL carries every parameter the service reads", () => {
  const url = new URL(
    endSessionUrl(config, { idTokenHint: "id.token.here", postLogoutRedirectUri: "/goodbye", state: "s1" }),
  );
  assertEquals(`${url.origin}${url.pathname}`, "https://accounts.toddgriffin.me/oidc/session/end");
  assertEquals(Object.fromEntries(url.searchParams), {
    client_id: "boggledygook",
    id_token_hint: "id.token.here",
    post_logout_redirect_uri: "https://www.boggledygook.com/goodbye",
    state: "s1",
  });
  assertEquals(Object.fromEntries(new URL(endSessionUrl(config)).searchParams), { client_id: "boggledygook" });
  const offSite = new URL(endSessionUrl(config, { postLogoutRedirectUri: "https://evil.example/" }));
  assertEquals(offSite.searchParams.has("post_logout_redirect_uri"), false);
});

Deno.test("post-logout targets stay on your site", () => {
  assertEquals(postLogoutTarget(config, "/"), "https://www.boggledygook.com/");
  assertEquals(
    postLogoutTarget(config, "https://www.boggledygook.com/bye?x=1"),
    "https://www.boggledygook.com/bye?x=1",
  );
  for (
    const bad of [
      "//evil.example/",
      "https://evil.example/",
      "http://www.boggledygook.com/",
      "https://www.boggledygook.com.evil.example/",
      "https://user@www.boggledygook.com/",
      "/\\evil.example",
      "goodbye",
      "",
    ]
  ) {
    assertEquals(postLogoutTarget(config, bad), null, JSON.stringify(bad));
  }
});

Deno.test("signOut revokes the refresh token and returns the end-session URL", async () => {
  const seen: string[] = [];
  let status = 200;
  const accounts = new Accounts(config, {
    keys: jwks,
    fetch: async (input, init) => {
      const req = new Request(input, init);
      seen.push(`${req.method} ${new URL(req.url).pathname} ${await req.text()}`);
      return new Response(null, { status });
    },
  });
  const ok = await accounts.signOut("rt1", { postLogoutRedirectUri: "/" });
  assertEquals(ok, {
    url: endSessionUrl(config, { postLogoutRedirectUri: "/" }),
    revokeError: null,
  });
  assertEquals(seen, ["POST /oidc/token/revocation token=rt1&token_type_hint=refresh_token"]);

  status = 503;
  const failed = await accounts.signOut("rt1");
  assertEquals([failed.url, failed.revokeError?.kind, failed.revokeError?.status], [
    endSessionUrl(config),
    "api",
    503,
  ]);

  seen.length = 0;
  assertEquals((await accounts.signOut(null)).revokeError, null);
  assertEquals(seen, []);
});

// A JWKS endpoint whose keys and status the test controls, behind a clock the test moves.
async function rotatingKeys() {
  const second = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  const newKey = { ...(await crypto.subtle.exportKey("jwk", second.publicKey)), kid: "k2", alg: "ES256", use: "sig" };
  const state = { published: { keys: [...jwks.keys] } as JwkSet, status: 200, hits: 0, clock: 0 };
  const fetcher = async () => {
    state.hits++;
    await new Promise((resolve) => setTimeout(resolve, 10)); // slow enough that concurrent lookups overlap
    return state.status === 200 ? Response.json(state.published) : new Response("down", { status: state.status });
  };
  const keys = Keys.remote("https://accounts.example/oidc/jwks", fetcher, () => state.clock);
  return { keys, state, publishNew: () => state.published.keys.push(newKey) };
}

Deno.test("a key published after the last fetch is found by refetching", async () => {
  const { keys, state, publishNew } = await rotatingKeys();
  await keys.get("k1");
  publishNew();
  state.clock += 61_000;
  await keys.get("k2");
  assertEquals(state.hits, 2);
});

Deno.test("unknown kids refetch at most once a minute", async () => {
  const { keys, state, publishNew } = await rotatingKeys();
  await keys.get("k1");
  // Within a minute of the last fetch: no refetch, even for a key that now exists.
  publishNew();
  await assertRejects(() => keys.get("k2"), AccountsError);
  assertEquals(state.hits, 1);
  state.clock += 61_000;
  for (const kid of ["made-up-1", "made-up-2", "made-up-3"]) {
    assertEquals((await assertRejects(() => keys.get(kid), AccountsError)).kind, "invalid_token");
  }
  assertEquals(state.hits, 2);
  await keys.get("k2"); // fetched by the made-up-1 refetch
  assertEquals(state.hits, 2);
});

Deno.test("a failed refetch still counts against the minute", async () => {
  const { keys, state } = await rotatingKeys();
  await keys.get("k1");
  state.clock += 61_000;
  state.status = 503;
  assertEquals((await assertRejects(() => keys.get("k2"), AccountsError)).kind, "http");
  assertEquals((await assertRejects(() => keys.get("k2"), AccountsError)).kind, "invalid_token");
  assertEquals(state.hits, 2);
  await keys.get("k1");
});

Deno.test("a stale cache is refreshed even for a known kid", async () => {
  const { keys, state } = await rotatingKeys();
  await keys.get("k1");
  state.published = { keys: [] }; // k1 retired
  state.clock += 3600_001;
  await assertRejects(() => keys.get("k1"), AccountsError);
  assertEquals(state.hits, 2);
});

Deno.test("concurrent lookups share one fetch, and its failure", async () => {
  const ok = await rotatingKeys();
  await Promise.all(Array.from({ length: 8 }, () => ok.keys.get("k1")));
  assertEquals(ok.state.hits, 1);

  const down = await rotatingKeys();
  down.state.status = 503;
  const results = await Promise.allSettled(Array.from({ length: 8 }, () => down.keys.get("k1")));
  assertEquals(results.map((r) => r.status), Array(8).fill("rejected"));
  assertEquals(down.state.hits, 1);
});

Deno.test("blocks and reports reach the service with the right credentials", async () => {
  const calls: string[] = [];
  const accounts = new Accounts(config, {
    keys: jwks,
    fetch: async (input, init) => {
      const req = new Request(input, init);
      calls.push(`${req.method} ${new URL(req.url).pathname} ${req.headers.get("authorization")} ${await req.text()}`);
      if (req.url.endsWith("/blocks")) {
        return Response.json({
          blocked: [{ sub: "u2", username: "rude_bob", name: null, picture: null, since: "s" }],
          hidden: ["u2", "u3"],
        });
      }
      return new Response(null, { status: 204 });
    },
  });
  const friends = accounts.friends("tok");
  assertEquals(await friends.blocks(), {
    blocked: [{ sub: "u2", username: "rude_bob", name: null, picture: null, since: "s" }],
    hidden: ["u2", "u3"],
  });
  await friends.report("u2", "name");
  await friends.report("u2", "other", "spam");
  await accounts.reportAsGuest({ sub: "u2", reason: "harassment", guestId: "g1", guestIp: "203.0.113.9" });
  assertEquals(calls, [
    "GET /api/v1/blocks Bearer tok ",
    'POST /api/v1/reports Bearer tok {"sub":"u2","reason":"name"}',
    'POST /api/v1/reports Bearer tok {"sub":"u2","reason":"other","note":"spam"}',
    `POST /api/v1/reports Basic ${
      btoa("boggledygook:s3cret")
    } {"sub":"u2","reason":"harassment","guest_id":"g1","guest_ip":"203.0.113.9"}`,
  ]);
});

Deno.test("a public client can't report for guests", async () => {
  const accounts = new Accounts({ ...config, clientSecret: null }, { keys: jwks, fetch: () => Promise.reject() });
  const e = await assertRejects(() => accounts.reportAsGuest({ sub: "u2", reason: "name" }), AccountsError);
  assertEquals(e.kind, "config");
});

Deno.test("accountUrl tags links with the client", () => {
  const c = toddgriffin("boggledygook", null, "https://www.boggledygook.com/auth/callback");
  assertEquals(accountUrl(c), "https://accounts.toddgriffin.me/account?client=boggledygook");
  assertEquals(
    accountUrl(c, "account/sessions"),
    "https://accounts.toddgriffin.me/account/sessions?client=boggledygook",
  );
  assertEquals(
    accountUrl(c, "/account?tab=1#top"),
    "https://accounts.toddgriffin.me/account?tab=1&client=boggledygook#top",
  );
  const local = { issuer: "http://localhost:8790/oidc/", clientId: "a b", clientSecret: null, redirectUri: "" };
  assertEquals(accountUrl(local, "/account"), "http://localhost:8790/account?client=a%20b");
});
