import { assert, assertEquals, assertNotEquals, assertRejects } from "@std/assert";
import {
  Accounts,
  AccountsError,
  hasScope,
  type JwkSet,
  parseCallback,
  pkceChallenge,
  safeReturnTo,
  startLogin,
  toddgriffin,
  verifyWebhook,
  VERSION,
} from "./mod.ts";
import { base64url, utf8 } from "./encoding.ts";

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

Deno.test("versions agree", async () => {
  const deno = JSON.parse((await Deno.readTextFile(new URL("./deno.jsonc", import.meta.url))).replace(/\/\/.*$/gm, ""));
  const npm = JSON.parse(await Deno.readTextFile(new URL("./package.json", import.meta.url)));
  assertEquals([deno.version, npm.version], [VERSION, VERSION]);
});
