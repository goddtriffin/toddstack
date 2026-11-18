import { assert, assertEquals, assertNotEquals, assertRejects, assertThrows } from "@std/assert";
import {
  Accounts,
  AccountsError,
  accountUrl,
  endSessionUrl,
  hasScope,
  isKnownWebhookEventType,
  isReview,
  type JwkSet,
  LOCAL_BASE_URL,
  localConfig,
  parseCallback,
  pkceChallenge,
  postLogoutTarget,
  safeReturnTo,
  startLogin,
  toddgriffin,
  toddgriffinAt,
  verifyWebhook,
  VERSION,
  visibilityOf,
  WEBHOOK_EVENT_TYPES,
} from "./mod.ts";
import { base64url, utf8 } from "./encoding.ts";
import { readReceipt } from "./notifications.ts";
import { Keys } from "./jwks.ts";
import { type RetryPolicy, send } from "./retry.ts";

// Retries as READ does them, with millisecond pauses so the tests stay fast.
const FAST: RetryPolicy = {
  attempts: 3,
  attemptTimeoutMs: 500,
  baseDelayMs: 10,
  maxDelayMs: 20,
  maxRetryAfterMs: 2_000,
  retry429: true,
  budgetMs: 4_000,
};

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

Deno.test("session.signed_out verifies with sub and sid, and a bad signature is refused", async () => {
  const body = '{"id":"e5","type":"session.signed_out","created_at":1700000000,"data":{"sub":"u1","sid":"s-1"}}';
  const event = await verifyWebhook("secret", await sign("secret", 1_700_000_000, body), body, 1_700_000_000);
  assertEquals(event, {
    id: "e5",
    type: "session.signed_out",
    createdAt: 1_700_000_000,
    data: { sub: "u1", sid: "s-1" },
  });
  assert(isKnownWebhookEventType(event.type));
  const e = await assertRejects(
    async () => verifyWebhook("secret", await sign("other", 1_700_000_000, body), body, 1_700_000_000),
    AccountsError,
  );
  assertEquals(e.message, "webhook rejected: signature mismatch");
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

Deno.test("an empty secret is refused before any signature is checked", async () => {
  const header = `t=1700000000,v1=${"00".repeat(32)}`;
  for (const secret of ["", new Uint8Array()]) {
    const e = await assertRejects(() => verifyWebhook(secret, header, BODY, 1_700_000_000), AccountsError);
    assertEquals([e.kind, e.message], ["invalid_webhook", "webhook rejected: empty secret"]);
  }
});

Deno.test("any one of several signatures may match during a secret rotation", async () => {
  const oldV1 = (await sign("old-secret", 1_700_000_000, BODY)).split("v1=")[1];
  const newV1 = (await sign("new-secret", 1_700_000_000, BODY)).split("v1=")[1];
  // Either secret verifies, whichever position its signature is in.
  for (const header of [`t=1700000000,v1=${oldV1},v1=${newV1}`, `t=1700000000,v1=${newV1},v1=${oldV1}`]) {
    for (const secret of ["old-secret", "new-secret"]) {
      assertEquals((await verifyWebhook(secret, header, BODY, 1_700_000_010)).id, "e1");
    }
  }
  // A malformed signature beside the right one is skipped, not fatal.
  assertEquals(
    (await verifyWebhook("new-secret", `v1=zz,t=1700000000,v1=${newV1},v1=00`, BODY, 1_700_000_010)).id,
    "e1",
  );
});

Deno.test("several signatures that all mismatch, or all malformed, are refused", async () => {
  const a = (await sign("a", 1_700_000_000, BODY)).split("v1=")[1];
  const b = (await sign("b", 1_700_000_000, BODY)).split("v1=")[1];
  const mismatch = await assertRejects(
    () => verifyWebhook("c", `t=1700000000,v1=${a},v1=${b}`, BODY, 1_700_000_000),
    AccountsError,
  );
  assertEquals(mismatch.message, "webhook rejected: signature mismatch");
  const malformed = await assertRejects(
    () => verifyWebhook("a", "t=1700000000,v1=zz,v1=abc", BODY, 1_700_000_000),
    AccountsError,
  );
  assertEquals(malformed.message, "webhook rejected: malformed signature header");
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
    "session.signed_out",
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

Deno.test("signOut carries the hint, redirect and state, and localOnly gives no URL", async () => {
  const seen: string[] = [];
  const accounts = new Accounts(config, {
    keys: jwks,
    fetch: (input, init) => {
      seen.push(new URL(new Request(input, init).url).pathname);
      return Promise.resolve(new Response(null));
    },
  });
  const { url } = await accounts.signOut("rt1", { idTokenHint: "a.b.c", postLogoutRedirectUri: "/bye", state: "s1" });
  const params = Object.fromEntries(new URL(url!).searchParams);
  assertEquals(params, {
    client_id: config.clientId,
    id_token_hint: "a.b.c",
    post_logout_redirect_uri: "https://www.boggledygook.com/bye",
    state: "s1",
  });
  assertEquals(new URL(url!).origin + new URL(url!).pathname, `${config.issuer}/session/end`);
  seen.length = 0;
  const local = await accounts.signOut("rt1", { idTokenHint: "a.b.c", localOnly: true });
  assertEquals(local, { url: null, revokeError: null });
  assertEquals(seen, ["/oidc/token/revocation"]);
});

Deno.test("the browser session id is exposed when the ID token carries one", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  assertEquals((await accounts.verifyIdToken(await jwt(idClaims("n")), "n")).sid, undefined);
  const claims = await accounts.verifyIdToken(await jwt({ ...idClaims("n"), sid: "sid-123" }), "n");
  assertEquals(claims.sid, "sid-123");
});

Deno.test("visibility is normal when the claim is absent and review for a review account", async () => {
  const accounts = new Accounts(config, { keys: jwks });
  const normal = await accounts.verifyIdToken(await jwt(idClaims("n")), "n");
  assertEquals([normal.visibility, isReview(normal)], ["normal", false]);
  const review = await accounts.verifyIdToken(
    await jwt({ ...idClaims("n"), toddgriffin_visibility: "review" }),
    "n",
  );
  assertEquals([review.visibility, isReview(review)], ["review", true]);
  assertEquals([visibilityOf(undefined), visibilityOf("review"), visibilityOf("newer")], [
    "normal",
    "review",
    "normal",
  ]);
});

Deno.test("a user.updated webhook carries the visibility", async () => {
  const body = '{"id":"e8","type":"user.updated","created_at":1700000000,"data":{"sub":"u1","visibility":"review"}}';
  const event = await verifyWebhook("secret", await sign("secret", 1_700_000_000, body), body, 1_700_000_010);
  assertEquals(event.data.visibility, "review");
});

Deno.test("senderSub is sent as sender_sub only when set", async () => {
  const { fetcher, calls } = recording(Response.json(DONE, { status: 202 }), Response.json(DONE, { status: 202 }));
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  await accounts.sendNotification({ recipient: { user: "u1" }, type: "k", title: "t" });
  await accounts.sendNotification({
    recipient: { user: "u1" },
    type: "k",
    title: "t",
    senderSub: "2f1c6a52-8f65-4c6b-9d8e-0a1b2c3d4e5f",
  });
  assertEquals("sender_sub" in JSON.parse(calls[0].body), false);
  assertEquals(JSON.parse(calls[1].body).sender_sub, "2f1c6a52-8f65-4c6b-9d8e-0a1b2c3d4e5f");
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
  const keys = Keys.remote("https://accounts.example/oidc/jwks", fetcher, () => state.clock, FAST);
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
  // The first fetch, then one refetch of three attempts (a 503 is retried); the second lookup fetches nothing.
  assertEquals(state.hits, 4);
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
  // One shared fetch: its three attempts, not three per caller.
  assertEquals(down.state.hits, 3);
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

// A fake accounts API that answers request `n` with reply `n` (the last repeats) and records each call.
function recording(...replies: Response[]) {
  const calls: { method: string; path: string; authorization: string | null; body: string }[] = [];
  const fetcher = async (input: RequestInfo | URL, init?: RequestInit) => {
    const req = new Request(input, init);
    calls.push({
      method: req.method,
      path: new URL(req.url).pathname,
      authorization: req.headers.get("authorization"),
      body: await req.text(),
    });
    return replies[Math.min(calls.length, replies.length) - 1].clone();
  };
  return { fetcher: fetcher as typeof fetch, calls };
}

const DONE = {
  id: "b1",
  status: "done",
  recipients: 1,
  result: "delivered",
  counts: { delivered: 1, scheduled: 0, not_delivered: 0, dropped_runaway: 0 },
};

Deno.test("a send goes to the API with the client credentials and reads the receipt", async () => {
  const { fetcher, calls } = recording(Response.json(DONE, { status: 202 }));
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  const receipt = await accounts.sendNotification({
    recipient: { user: "u1" },
    type: "streak_at_risk",
    title: "Your streak ends tonight",
    sendAtLocal: "20:00",
    collapseKey: "streak",
    idempotencyKey: "daily-2026-11-15",
  });
  assertEquals(receipt, {
    id: "b1",
    status: "done",
    recipients: 1,
    counts: { delivered: 1, scheduled: 0, notDelivered: 0, droppedRunaway: 0 },
    result: "delivered",
    replayed: false,
  });
  assertEquals(calls.length, 1);
  assertEquals([calls[0].method, calls[0].path], ["POST", "/api/v1/notifications"]);
  assertEquals(calls[0].authorization, `Basic ${btoa("boggledygook:s3cret")}`);
  assertEquals(JSON.parse(calls[0].body), {
    recipient: { user: "u1" },
    type: "streak_at_risk",
    title: "Your streak ends tonight",
    body: "",
    send_at_local: "20:00",
    collapse_key: "streak",
    idempotency_key: "daily-2026-11-15",
  });
});

Deno.test("every recipient uses the service shape, and a fan-out replay is polled by id", async () => {
  const queued = { id: "b2", status: "queued", recipients: 40, counts: {}, replayed: true };
  const finished = {
    id: "b2",
    status: "done",
    recipients: 40,
    counts: { delivered: 37, scheduled: 0, not_delivered: 3, dropped_runaway: 0 },
  };
  const { fetcher, calls } = recording(
    Response.json(queued, { status: 202 }),
    Response.json(queued, { status: 202 }),
    Response.json(finished),
  );
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  const sent = await accounts.sendNotification({
    recipient: { allPlayers: true },
    type: "daily_ready",
    title: "Today's daily is ready",
    idempotencyKey: "daily-2026-11-15",
  });
  assertEquals([sent.status, sent.replayed, sent.result], ["queued", true, null]);
  await accounts.sendNotification({ recipient: { friendsOf: "u1" }, type: "k", title: "t" });
  assertEquals([JSON.parse(calls[0].body).recipient, JSON.parse(calls[1].body).recipient], [
    { all_players: true },
    { friends_of: "u1" },
  ]);
  const status = await accounts.notificationStatus("b2");
  assertEquals([status.status, status.counts, status.replayed], [
    "done",
    { delivered: 37, scheduled: 0, notDelivered: 3, droppedRunaway: 0 },
    false,
  ]);
  assertEquals([calls[2].method, calls[2].path], ["GET", "/api/v1/notifications/b2"]);
});

Deno.test("friendsOf posts the distinct subs as the project and returns the map", async () => {
  const friends = { u1: ["u2", "u3"], u2: ["u1"], u4: [] };
  const { fetcher, calls } = recording(Response.json({ friends }));
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  assertEquals(await accounts.friendsOf(["u2", "u1", "u4", "u1"]), friends);
  assertEquals([calls[0].method, calls[0].path, JSON.parse(calls[0].body)], [
    "POST",
    "/api/v1/friends-of",
    { subs: ["u2", "u1", "u4"] },
  ]);
});

Deno.test("friendsOf lowercases subs before deduping", async () => {
  const { fetcher, calls } = recording(Response.json({ friends: {} }));
  await new Accounts(config, { keys: jwks, fetch: fetcher }).friendsOf(["ABC-1", "abc-1", "Abc-1"]);
  assertEquals(JSON.parse(calls[0].body), { subs: ["abc-1"] });
});

Deno.test("friendsOf is retried after a 503 and refusals become typed errors", async () => {
  const flaky = recording(new Response("{}", { status: 503 }), Response.json({ friends: { u1: [] } }));
  assertEquals(await new Accounts(config, { keys: jwks, fetch: flaky.fetcher }).friendsOf(["u1"]), { u1: [] });
  assertEquals(flaky.calls.length, 2);
  for (const [status, kind] of [[404, "not_found"], [400, "api"], [429, "rate_limited"]] as const) {
    const { fetcher } = recording(Response.json({ error: "no" }, { status }));
    const error = await assertRejects(
      () => new Accounts(config, { keys: jwks, fetch: fetcher }).friendsOf(["u1"]),
      AccountsError,
    );
    assertEquals(error.kind, kind);
  }
  const bad = recording(Response.json({}));
  await assertRejects(() => new Accounts(config, { keys: jwks, fetch: bad.fetcher }).friendsOf(["u1"]), AccountsError);
});

Deno.test("friendsOf needs the client secret", async () => {
  const { fetcher, calls } = recording(Response.json({ friends: {} }));
  const error = await assertRejects(
    () => new Accounts({ ...config, clientSecret: null }, { keys: jwks, fetch: fetcher }).friendsOf(["u1"]),
    AccountsError,
  );
  assertEquals([error.kind, calls.length], ["config", 0]);
});

Deno.test("cancelling names the key and optionally the player", async () => {
  const { fetcher, calls } = recording(Response.json({ cancelled: 2 }));
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  assertEquals(await accounts.cancelNotifications("streak", "u1"), 2);
  assertEquals(await accounts.cancelNotifications("streak"), 2);
  assertEquals(calls.map((c) => [c.method, c.path, JSON.parse(c.body)]), [
    ["POST", "/api/v1/notifications/cancel", { collapse_key: "streak", user: "u1" }],
    ["POST", "/api/v1/notifications/cancel", { collapse_key: "streak" }],
  ]);
});

Deno.test("notification refusals become typed errors", async () => {
  const cases = [
    [404, "not_found", "Not found."],
    [422, "unprocessable", "That idempotency key was used for a different request."],
    [429, "rate_limited", "Too many notifications."],
    [400, "api", "title must be 1-80 characters."],
  ] as const;
  for (const [status, kind, message] of cases) {
    const { fetcher } = recording(Response.json({ error: message }, { status }));
    const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
    const e = await assertRejects(
      () => accounts.sendNotification({ recipient: { user: "u1" }, type: "k", title: "t" }),
      AccountsError,
    );
    assertEquals([e.kind, e.status, e.message], [kind, status, `accounts API returned ${status}: ${message}`]);
  }
  const { fetcher } = recording(Response.json({ error: "Not found." }, { status: 404 }));
  const e = await assertRejects(
    () => new Accounts(config, { keys: jwks, fetch: fetcher }).notificationStatus("nope"),
    AccountsError,
  );
  assertEquals(e.kind, "not_found");
});

Deno.test("a send without a key is sent once on a 503; with a key it is retried", async () => {
  const unkeyed = recording(new Response("down", { status: 503 }), Response.json(DONE, { status: 202 }));
  const e = await assertRejects(
    () =>
      new Accounts(config, { keys: jwks, fetch: unkeyed.fetcher }).sendNotification({
        recipient: { user: "u1" },
        type: "k",
        title: "t",
      }),
    AccountsError,
  );
  assertEquals([e.kind, e.status, unkeyed.calls.length], ["api", 503, 1]);

  const keyed = recording(new Response("down", { status: 503 }), Response.json(DONE, { status: 202 }));
  const receipt = await new Accounts(config, { keys: jwks, fetch: keyed.fetcher }).sendNotification({
    recipient: { user: "u1" },
    type: "k",
    title: "t",
    idempotencyKey: "k1",
  });
  assertEquals([receipt.id, keyed.calls.length], ["b1", 2]);
  assertEquals(keyed.calls[0].body, keyed.calls[1].body);
});

Deno.test("send retries a keyed write but a refused one is an answer", async () => {
  const { fetcher, seen } = scripted(status(503), status(422), status(200));
  const response = await send(fetcher, "https://accounts.example/x", { method: "POST", body: "{}" }, FAST, true);
  assertEquals([response.status, seen], [422, ["POST", "POST"]]);
});

Deno.test("a public client can't send notifications", async () => {
  const accounts = new Accounts({ ...config, clientSecret: null }, { keys: jwks, fetch: () => Promise.reject() });
  const bad = [
    () => accounts.sendNotification({ recipient: { user: "u1" }, type: "k", title: "t", idempotencyKey: "k1" }),
    () => accounts.cancelNotifications("streak"),
    () => accounts.notificationStatus("b1"),
  ];
  for (const call of bad) assertEquals((await assertRejects(call, AccountsError)).kind, "config");
});

const DEVICE = {
  sub: "u1",
  platform: "ios",
  token: "ab12cd34",
  appId: "me.toddgriffin.boggledygook",
  environment: "production",
} as const;

Deno.test("registering a device posts it with the client credentials, and unregistering deletes it", async () => {
  const { fetcher, calls } = recording(new Response(null, { status: 204 }));
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  await accounts.registerDevice(DEVICE);
  await accounts.unregisterDevice("android", "fcm-token");
  assertEquals(
    calls.map((c) => [c.method, c.path, c.authorization, JSON.parse(c.body)]),
    [
      ["POST", "/api/v1/devices", `Basic ${btoa("boggledygook:s3cret")}`, {
        sub: "u1",
        platform: "ios",
        token: "ab12cd34",
        app_id: "me.toddgriffin.boggledygook",
        environment: "production",
      }],
      ["DELETE", "/api/v1/devices", `Basic ${btoa("boggledygook:s3cret")}`, {
        platform: "android",
        token: "fcm-token",
      }],
    ],
  );
});

Deno.test("device refusals become typed errors", async () => {
  const cases = [
    [409, "conflict", "That device token is registered to another project."],
    [404, "not_found", "Not found."],
    [403, "api", "That app id isn't registered to this project."],
  ] as const;
  for (const [status, kind, message] of cases) {
    const { fetcher } = recording(Response.json({ error: message }, { status }));
    const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
    const e = await assertRejects(() => accounts.registerDevice(DEVICE), AccountsError);
    assertEquals([e.kind, e.status, e.message], [kind, status, `accounts API returned ${status}: ${message}`]);
  }
});

Deno.test("a device call is retried after a 503 because it is idempotent", async () => {
  const { fetcher, calls } = recording(new Response("down", { status: 503 }), new Response(null, { status: 204 }));
  await new Accounts(config, { keys: jwks, fetch: fetcher }).registerDevice(DEVICE);
  assertEquals(calls.length, 2);
});

Deno.test("a public client can't register devices", async () => {
  const accounts = new Accounts({ ...config, clientSecret: null }, { keys: jwks, fetch: () => Promise.reject() });
  const bad = [() => accounts.registerDevice(DEVICE), () => accounts.unregisterDevice("ios", "ab12")];
  for (const call of bad) assertEquals((await assertRejects(call, AccountsError)).kind, "config");
});

Deno.test("the time zone hint is a PUT with the person's token", async () => {
  const { fetcher, calls } = recording(new Response(null, { status: 204 }));
  const accounts = new Accounts(config, { keys: jwks, fetch: fetcher });
  await accounts.friends("tok").setTimeZone("America/Chicago");
  assertEquals(calls, [{
    method: "PUT",
    path: "/api/v1/me/time-zone",
    authorization: "Bearer tok",
    body: '{"time_zone":"America/Chicago"}',
  }]);
});

Deno.test("the inbox and event answers carry the new fields, and an old service's still read", async () => {
  const { fetcher } = recording(
    Response.json({
      items: [{
        id: "i1",
        client_id: null,
        from: null,
        from_username: null,
        kind: "friend_accepted",
        title: "t",
        body: "",
        url: null,
        created_at: "c",
        read_at: null,
      }],
      unread: { project: 0, account: 1, total: 1 },
    }),
    Response.json({
      items: [{
        id: "i2",
        from: null,
        from_username: null,
        kind: "k",
        title: "t",
        body: "",
        url: null,
        created_at: "c",
        read_at: null,
      }],
    }),
    Response.json({
      delivered: 2,
      dropped: 1,
      counts: { delivered: 2, scheduled: 0, not_delivered: 0, dropped_runaway: 1 },
    }, { status: 202 }),
    Response.json({ delivered: 2 }, { status: 202 }),
  );
  const friends = new Accounts(config, { keys: jwks, fetch: fetcher }).friends("tok");
  const inbox = await friends.inboxWithUnread(20);
  assertEquals([inbox.items[0].clientId, inbox.unread], [null, { project: 0, account: 1, total: 1 }]);
  const old = await friends.inboxWithUnread(20);
  assertEquals([old.items[0].clientId, old.unread], [null, null]);
  const event = { kind: "k", title: "t" };
  assertEquals(await friends.postEventResult(event), {
    delivered: 2,
    dropped: 1,
    counts: { delivered: 2, scheduled: 0, notDelivered: 0, droppedRunaway: 1 },
  });
  assertEquals(await friends.postEventResult(event), { delivered: 2, dropped: 0, counts: null });
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

// A server that answers each request with the next scripted response, and records the methods it saw.
function scripted(...responses: (() => Response)[]) {
  const seen: string[] = [];
  const fetcher = (input: RequestInfo | URL, init?: RequestInit) => {
    seen.push(new Request(input, init).method);
    const next = responses[Math.min(seen.length, responses.length) - 1];
    return Promise.resolve(next());
  };
  return { fetcher: fetcher as typeof fetch, seen };
}
const status = (code: number, headers: HeadersInit = {}) => () => new Response("x", { status: code, headers });

Deno.test("a GET is retried after a 503 and returns the success", async () => {
  const { fetcher, seen } = scripted(status(503), () => Response.json({ ok: true }));
  const response = await send(fetcher, "https://accounts.example/x", {}, FAST);
  assertEquals([response.status, seen.length], [200, 2]);
});

Deno.test("a GET gives up after the last attempt and returns its answer", async () => {
  const { fetcher, seen } = scripted(status(502));
  assertEquals((await send(fetcher, "https://accounts.example/x", {}, FAST)).status, 502);
  assertEquals(seen.length, 3);
});

Deno.test("a 400 is an answer, not retried", async () => {
  const { fetcher, seen } = scripted(status(400), status(200));
  assertEquals((await send(fetcher, "https://accounts.example/x", {}, FAST)).status, 400);
  assertEquals(seen.length, 1);
});

Deno.test("a POST is sent once, even on a 503", async () => {
  const { fetcher, seen } = scripted(status(503), status(200));
  const response = await send(fetcher, "https://accounts.example/x", { method: "POST", body: "a=1" }, FAST);
  assertEquals([response.status, seen], [503, ["POST"]]);
});

Deno.test("a network error on a GET is retried", async () => {
  let calls = 0;
  const fetcher =
    (() =>
      ++calls === 1
        ? Promise.reject(new TypeError("connection refused"))
        : Promise.resolve(Response.json({}))) as typeof fetch;
  assertEquals((await send(fetcher, "https://accounts.example/x", {}, FAST)).status, 200);
  assertEquals(calls, 2);
});

Deno.test("Retry-After is honoured up to the cap, and a longer one stops retrying", async () => {
  const short = scripted(status(429, { "retry-after": "1" }), status(200));
  const started = Date.now();
  assertEquals((await send(short.fetcher, "https://accounts.example/x", {}, FAST)).status, 200);
  assert(Date.now() - started >= 950, "waited out the Retry-After");
  assertEquals(short.seen.length, 2);

  const long = scripted(status(503, { "retry-after": "30" }), status(200));
  const quick = Date.now();
  assertEquals((await send(long.fetcher, "https://accounts.example/x", {}, FAST)).status, 503);
  assert(Date.now() - quick < 500, "did not wait 30 s");
  assertEquals(long.seen.length, 1);
});

Deno.test("friends reads retry through Accounts; writes and refresh do not", async () => {
  const seen: string[] = [];
  let failNext = 0;
  const accounts = new Accounts(config, {
    keys: jwks,
    fetch: (input, init) => {
      const req = new Request(input, init);
      seen.push(`${req.method} ${new URL(req.url).pathname}`);
      if (failNext > 0) {
        failNext--;
        return Promise.resolve(new Response("down", { status: 503 }));
      }
      return Promise.resolve(Response.json({ friends: [] }));
    },
  });
  failNext = 1;
  await accounts.friends("at1").list();
  assertEquals(seen, ["GET /api/v1/friends", "GET /api/v1/friends"]);

  seen.length = 0;
  failNext = 1;
  await assertRejects(() => accounts.refresh("rt1"), AccountsError);
  assertEquals(seen, ["POST /oidc/token"]);
});

Deno.test("a receipt with an unknown status or result reads as unknown, and one without an id is an api error", () => {
  const receipt = readReceipt({ id: "b9", status: "paused", recipients: 1, counts: {}, result: "snoozed" });
  assertEquals([receipt.status, receipt.result], ["unknown", "unknown"]);
  const e = assertThrows(() => readReceipt({ status: "done" }), AccountsError);
  assertEquals([e.kind, e.status], ["api", 200]);
});

Deno.test("a 429 is final for a keyed send and for notification status, and a 5xx is still retried", async () => {
  const keyed = recording(new Response("slow", { status: 429 }), Response.json(DONE, { status: 202 }));
  const e = await assertRejects(
    () =>
      new Accounts(config, { keys: jwks, fetch: keyed.fetcher }).sendNotification({
        recipient: { user: "u1" },
        type: "k",
        title: "t",
        idempotencyKey: "k1",
      }),
    AccountsError,
  );
  assertEquals([e.kind, keyed.calls.length], ["rate_limited", 1]);

  const poll = recording(new Response("slow", { status: 429 }), Response.json(DONE));
  const e2 = await assertRejects(
    () => new Accounts(config, { keys: jwks, fetch: poll.fetcher }).notificationStatus("b1"),
    AccountsError,
  );
  assertEquals([e2.kind, poll.calls.length], ["rate_limited", 1]);

  const flaky = recording(new Response("down", { status: 503 }), Response.json(DONE));
  const ok = await new Accounts(config, { keys: jwks, fetch: flaky.fetcher }).notificationStatus("b1");
  assertEquals([ok.id, flaky.calls.length], ["b1", 2]);
});

Deno.test("notificationStatus percent-encodes the id", async () => {
  const { fetcher, calls } = recording(Response.json(DONE));
  await new Accounts(config, { keys: jwks, fetch: fetcher }).notificationStatus("a/b?c");
  assertEquals(calls[0].path.endsWith("/notifications/a%2Fb%3Fc"), true);
});

Deno.test("local mode derives everything from the local base", () => {
  const c = localConfig("app", null, "http://127.0.0.1:8992/auth/callback");
  assertEquals(LOCAL_BASE_URL, "http://127.0.0.1:8790");
  assertEquals(c.issuer, "http://127.0.0.1:8790/oidc");
  assertEquals(accountUrl(c, "/account"), "http://127.0.0.1:8790/account?client=app");
  new Accounts(c);
});

Deno.test("a base URL overrides the issuer", () => {
  assertEquals(toddgriffinAt("http://localhost:9000/", "app", null, "x").issuer, "http://localhost:9000/oidc");
  assertEquals(toddgriffinAt("http://[::1]:9000", "app", null, "x").issuer, "http://[::1]:9000/oidc");
  assertEquals(
    toddgriffinAt("https://staging.example.com", "app", null, "x").issuer,
    "https://staging.example.com/oidc",
  );
});

Deno.test("http off loopback is rejected", () => {
  for (
    const base of [
      "http://accounts.toddgriffin.me",
      "http://10.0.2.2:8790",
      "http://192.168.1.5:8790",
      "http://127.0.0.1.evil.example",
      "http://localhost@evil.com",
      "http://localhost.",
      "http://[::ffff:127.0.0.1]",
      "http://127.0.0.2",
      "ftp://127.0.0.1",
      "nonsense",
    ]
  ) {
    const error = assertThrows(() => toddgriffinAt(base, "app", null, "x"), AccountsError, "", base);
    assertEquals(error.kind, "config");
  }
  const hand = { issuer: "http://accounts.example.com/oidc", clientId: "app", clientSecret: null, redirectUri: "" };
  assertThrows(() => new Accounts(hand), AccountsError);
});

Deno.test("loopback hosts compare case-insensitively", () => {
  assertEquals(toddgriffinAt("http://LOCALHOST:8790", "app", null, "x").issuer, "http://LOCALHOST:8790/oidc");
});

Deno.test("the server-side Friends refuse ids that could leave their URL paths, without a request", async () => {
  let requests = 0;
  const accounts = new Accounts(config, { fetch: () => (requests++, Promise.resolve(new Response("{}"))) });
  const friends = accounts.friends("tok");
  for (const bad of ["../../oidc/x", "..", "a/b", "a?b=1", "a#b", "a%2Fb", ""]) {
    for (
      const call of [friends.accept, friends.decline, friends.cancel, friends.remove, friends.block, friends.unblock]
    ) {
      const error = await assertRejects(() => call.call(friends, bad), AccountsError);
      assertEquals(["api", 400], [error.kind, error.status], bad);
    }
    await assertRejects(() => friends.relationships(["u2", bad]), AccountsError);
  }
  assertEquals(0, requests);
});
